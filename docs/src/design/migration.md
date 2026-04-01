# Gateway Migration

This document describes how the server handles a chain migrating its settlement layer (SL) — either from L1 to the Gateway, or from the Gateway back to L1.

## Background

A ZKsync chain always settles somewhere: it commits, proves, and executes batches against a single settlement layer. That layer is discovered at startup by calling `getSettlementLayer()` on the L1 diamond proxy:

- `address(0)` → the chain is settling on L1 directly.
- Any other address → the chain is settling on the Gateway (that address is the Gateway diamond proxy).

A migration changes which layer the chain settles on. The two directions are:

- **L1 → Gateway** (`MigrateToGateway` event on L1).
- **Gateway → L1** (`MigrateFromGateway` event on L1).

Both directions are handled identically by the node.

## Key Invariant

At any point in time the node commits, proves, and executes batches on exactly one settlement layer. Submitting a commit transaction to the wrong layer after a migration causes it to be rejected or creates an inconsistent state. The migration machinery therefore ensures:

1. No new commit transactions are submitted to the old SL once migration is triggered.
2. The node restarts only after the in-flight pipeline is in a safe state.
3. After restart the node seamlessly picks up on the new SL.

## Components

Five components participate in migration handling. They are only active on protocol version ≥ v31.

### `GatewayMigrationWatcher` (`lib/l1_watcher`)

Watches the `ServerNotifier` contract on L1 for `MigrateToGateway` and `MigrateFromGateway` events. Both events carry an indexed `chainId` field; the watcher applies a `topic1` filter so it only receives events for the local chain.

On detection it does two things:

1. Writes `GatewayMigrationState::InProgress { migration_number }` to the shared watch channel so that `MigrationGate` knows a migration is underway and what migration number to look for.
2. Constructs a `SetSLChainId` system transaction with the new settlement layer chain ID and inserts it into `SlChainIdSubpool` so the sequencer will execute it in the next available block.

The starting L1 block for event scanning is found by binary search on `IChainAssetHandler::migrationNumber(chainId)`, which avoids re-processing old events after a restart.

### `SlChainIdSubpool` (`lib/mempool`)

A specialised mempool subpool that holds at most one pending `SetSLChainId` system transaction at a time. The sequencer drains it like any other subpool; `on_canonical_state_change` is called after each block is finalised to remove the executed transaction and record the migration number in the block context cursors.

### `MigrationGate` (`node/bin`)

A pipeline component inserted between `UpgradeGatekeeper` and the L1 commit sender (`L1Sender<CommitCommand>`). Under normal operation it is transparent; it only activates during migration.

For each incoming `L1SenderCommand<CommitCommand>` the gate inspects the `BatchMetadata`:

- If `set_sl_chain_id_migration_number` is `Some(n)` **and** the shared `GatewayMigrationState` is `InProgress { migration_number: n }`, this is the triggering batch.

When a triggering batch is detected the gate:

1. Records the batch number and sends it to `SettlementLayerWatcher` via the `migration_triggered` watch channel. This is done **before** pausing so that the watcher can immediately start checking preconditions.
2. Calls `wait_for(Stable)` on the `GatewayMigrationState` watch receiver, blocking all subsequent commit submissions until either migration is fully finalised (see `MigrationFinalizedWatcher`) or the node is restarted by `SettlementLayerWatcher`.

The triggering batch itself is forwarded only after the wait resolves (either on the restarted node, where the initial state is `Stable`, or in the unlikely case that `MigrationFinalizedWatcher` signals completion on the same node).

`BatchMetadata.set_sl_chain_id_migration_number` is populated by `seal_batch` in `node/bin/src/batcher/batch_builder.rs`: it scans all `ReplayRecord` transactions in the batch and records the migration number of the first `SetSLChainId` system transaction found, excluding the `u64::MAX` sentinel used for protocol upgrades.

### `SettlementLayerWatcher` (`lib/l1_watcher`)

Polls `getSettlementLayer()` on the L1 diamond proxy at regular intervals and terminates the process (via `std::process::exit(1)`) so that the process manager restarts the node. The crash is only triggered when **all three** of the following conditions are satisfied simultaneously:

| Condition | Why |
|-----------|-----|
| `getSettlementLayer()` has changed from the startup value | The L1 side of the migration has completed. |
| `migration_triggered` is `Some(N)` — the gate has detected the `SetSLChainId` batch | The L2 side of the migration (`SetSLChainId`) has been executed and is queued for commitment; the new SL chain ID is known at L2. |
| `get_total_batches_executed() ≥ N − 1` | All batches that existed before the migration batch are fully executed (committed, proved, and executed) on L1. This guarantees the old SL has no in-flight work the new SL would have to replay. |

Waiting for all three conditions prevents a premature crash that would leave partially-executed batches stranded on the old settlement layer.

### `MigrationFinalizedWatcher` (`lib/l1_watcher`)

Watches for `MigrationFinalized(uint256 indexed chainId, uint256 migrationNumber, ...)` events on `IChainAssetHandler` on the **current** settlement layer. The `chainId` index allows a `topic1` filter, so only events for the local chain are returned.

On detection it sends `GatewayMigrationState::Stable` to the shared watch channel, which unblocks `MigrationGate.wait_for(Stable)`.

On a node that started before the migration (SL = L1) and has not yet been restarted, `MigrationFinalizedWatcher` watches L1. If `MigrationFinalized` is emitted on the Gateway instead, it will not be seen here; the node will instead be restarted by `SettlementLayerWatcher`, and the new node (SL = Gateway) will have a fresh watcher pointing at Gateway.

The starting SL block for event scanning is also found by binary search on `IChainAssetHandler::migrationNumber`.

## Full Flow: L1 → Gateway

```
L1                              Node (old, SL=L1)             Node (new, SL=GW)
─────────────────────────────   ─────────────────────────────  ──────────────────────────────
Admin tx triggers migration
  └─► MigrateToGateway event
        │
        ├──────────────────────► GatewayMigrationWatcher
        │                           sets InProgress{M}
        │                           inserts SetSLChainId(GW, M)
        │                           into SlChainIdSubpool
        │
        │                        Sequencer executes SetSLChainId
        │                        in block X → batch N
        │                           │
        │                        MigrationGate sees batch N
        │                           sends migration_triggered=Some(N)
        │                           enters wait_for(Stable)
        │
getSettlementLayer() changes ──► SettlementLayerWatcher polls:
        │                           ✓ SL changed
        │                           ✓ migration_triggered = Some(N)
        │                           waiting for executed ≥ N-1...
        │
L1 executes batch N-1 ──────────► executed ≥ N-1 satisfied
                                    → std::process::exit(1)
                                                                 Node starts, SL=GW
                                                                 MigrationFinalizedWatcher
                                                                   watches GW for
                                                                   MigrationFinalized(chainId)
MigrationFinalized on GW ─────────────────────────────────────►    sets Stable
                                                                 MigrationGate unblocked
                                                                 commits resume on GW
```

## Shared State

All migration components communicate through two `tokio::sync::watch` channels created at startup:

| Channel | Type | Producer | Consumer |
|---------|------|----------|---------|
| `migration_state` | `GatewayMigrationState` | `GatewayMigrationWatcher` (→ `InProgress`), `MigrationFinalizedWatcher` (→ `Stable`) | `MigrationGate` |
| `migration_triggered` | `Option<u64>` | `MigrationGate` (→ `Some(batch_number)`) | `SettlementLayerWatcher` |

Both channels are created unconditionally (initial values: `Stable` and `None`) so that on pre-v31 chains the batcher pipeline compiles and runs unchanged — the senders are simply never written to.

## Startup Recovery

After a crash-restart the node calls `L1State::fetch`, which re-evaluates `getSettlementLayer()` and sets up providers accordingly. If the migration has already completed:

- `settlement_layer_address` will be non-zero, and the Gateway RPC URL must be configured.
- `GatewayMigrationWatcher` starts scanning from the block where `migrationNumber` first reached `next_cursors.migration_number` (determined by binary search), so it will re-detect the migration event if it has not yet processed it.
- `MigrationFinalizedWatcher` similarly starts from the first SL block where `migrationNumber` matches, and will immediately signal `Stable` if `MigrationFinalized` has already been emitted.
- `MigrationGate` starts with `migration_state = Stable` and `migration_triggered = None`; it will re-arm if `GatewayMigrationWatcher` re-detects the event.
