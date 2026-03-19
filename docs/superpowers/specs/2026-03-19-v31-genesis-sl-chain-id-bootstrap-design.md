# V31 Genesis Settlement Layer Chain ID Bootstrap

## Background

Protocol version 31 introduces `sl_chain_id` into the batch public input hash (see
`batch_info.rs::public_input_hash`). The L2 value of `currentSettlementLayerChainId` in
`SystemContext` (address `0x800b`) is what the prover reads. On any chain that starts at or
upgrades to v31, this L2 value must be initialised to match the actual settlement layer chain ID
before the first v31 batch is proved.

Prior to v31 the value is never set, so it is `0`. The gateway migration watcher (spawned when
`current_protocol_version >= v31`) handles subsequent changes via `MigrateToGateway` /
`MigrateFromGateway` events. But there is no mechanism to perform the initial population.

## Goal

At genesis, when the protocol version is v31 or higher, emit a `SetSLChainId` system transaction
that seeds `currentSettlementLayerChainId` with the correct value so that v31 batch proofs are
valid from batch 1 onwards.

## Scope

**In scope**: genesis only (`starting_block == 1`), protocol version >= v31.

**Not in scope**: in-run upgrades from v30 to v31. A v30 chain that upgrades to v31 must restart
for the upgrade transaction to take effect. After that restart `starting_block > 1` (the chain has
prior blocks), so the genesis injection path does not fire. That case is intentionally excluded to
keep this change minimal; it is expected to be handled separately if needed.

## Design

### `sl_chain_id` value

`node_startup_state.l1_state.sl_chain_id` is populated by `sl_provider.get_chain_id()` in
`L1State::fetch` (`lib/contract_interface/src/l1_discovery.rs`). When `gateway_rpc_url` is not
configured, `sl_provider` equals `l1_provider`, so `sl_chain_id` equals `l1_chain_id`. This is
always the chain ID of the current settlement layer at startup — the correct value to write into
`SystemContext`.

`L1State::fetch` also calls `validate_chain_ids`, which confirms that `sl_chain_id` is whitelisted
on the L1 Bridgehub. A chain ID of 0 cannot be whitelisted, so `sl_chain_id` is always non-zero
when startup proceeds past that validation.

### Sentinel migration number: `u64::MAX`

`SetSLChainId` carries a `migration_number` salt. This value propagates through the system as
follows:

1. `SlChainIdSubpool::on_canonical_state_change` returns `Option<u64>` (the last migration number
   seen) to the pool.
2. The pool propagates it as `PoolOutcome::last_migration_number`.
3. `BlockContextProvider::on_canonical_state_change` reads `outcome.last_migration_number` and, if
   `Some`, advances `next_migration_number`.
4. `next_migration_number` becomes `starting_migration_number` in the WAL record for subsequent
   blocks.
5. On restart, `starting_migration_number` from the first replay record becomes the
   `current_migration_number` passed to `GatewayMigrationWatcher`, which uses it to binary-search
   for its starting L1 block via `IChainAssetHandler.migrationNumber(chainId) >=
   current_migration_number`.

The bootstrap transaction must not disturb this tracking. We designate `u64::MAX` as a sentinel
meaning "genesis bootstrap, not a real gateway migration". No real migration will ever reach this
number. When `on_canonical_state_change` sees `migration_number == u64::MAX` it returns `None`,
leaving `next_migration_number` at `0`. The gateway migration watcher then binary-searches for the
first L1 block where `migrationNumber >= 0` (always true), i.e., block 0 — the correct starting
point before any real migration.

`BlockContextProvider` already handles `None` via an `if let Some(...)` guard, so no changes are
needed there. This filter is **load-bearing**: if the sentinel were propagated as `Some(u64::MAX)`,
`BlockContextProvider` would compute `u64::MAX + 1`, which panics in debug builds and silently
wraps to `0` in release — resetting `next_migration_number` incorrectly. The filter in
`on_canonical_state_change` is therefore a correctness requirement, not just a bookkeeping
convenience.

### Concurrency with `GatewayMigrationWatcher`

`GatewayMigrationWatcher` is spawned at startup **before** block 1 is executed — not after genesis
completes. It starts polling L1 immediately. If a real `MigrateToGateway` event arrives on L1
during the genesis block's execution window, the watcher calls `sl_chain_id_subpool.insert()`,
pushing the migration tx onto `pending_txs` while the bootstrap tx is already there.

`SlChainIdTransactionsStream` is intentionally a single-item stream: it serves one transaction
then closes. Step-by-step deque state in the race scenario (deque shown front→back):

1. Genesis injection: `insert(bootstrap)` → `push_front` → deque: `[bootstrap]`
2. Watcher fires concurrent migration: `insert(migration_tx)` → `push_front` →
   deque: `[migration_tx, bootstrap]`
3. Block 1 — `best_transactions_stream`: reads `pending_txs.back()` = bootstrap → serves it,
   stream closes.
4. Block 1 — `on_canonical_state_change`: calls `pop_back()` → removes bootstrap →
   deque: `[migration_tx]`.
5. Block 2 — `best_transactions_stream`: reads `pending_txs.back()` = migration_tx →
   serves it immediately (`StreamState::Pending`).

The migration tx is delivered in block 2 with no loss. This is the normal multi-queued behaviour
of the subpool, unchanged by this feature.

Additionally, because `starting_migration_number` stays at `0`, the watcher re-scans from L1
block 0 on any restart, ensuring any real migration event is also picked up from L1 history even
if a concurrent in-memory delivery was interrupted.

### Changes

#### 1. `lib/mempool/src/subpools/sl_chain_id.rs` — skip sentinel in `on_canonical_state_change`

```rust
if let SystemTxType::SetSLChainId(migration_number) = *tx.system_subtype() {
    // u64::MAX is the genesis bootstrap sentinel — not a real gateway migration.
    // Skipping it keeps starting_migration_number at 0 so the gateway migration
    // watcher starts its L1 binary search from block 0.
    if migration_number != u64::MAX {
        last_migration_number = Some(migration_number);
    }
}
```

#### 2. `node/bin/src/lib.rs` — inject bootstrap tx at genesis

In the existing `if starting_block == 1` block (where the genesis upgrade tx is inserted into
`upgrade_subpool`), also inject a bootstrap `SetSLChainId` when the genesis protocol version
is >= v31:

```rust
if genesis_upgrade.protocol_version >= ProtocolSemanticVersion::new(0, 31, 0) {
    let bootstrap = SystemTxEnvelope::set_sl_chain_id(
        node_startup_state.l1_state.sl_chain_id,
        u64::MAX, // genesis bootstrap sentinel — not a real migration number
    );
    sl_chain_id_subpool.insert(bootstrap).await;
}
```

`sl_chain_id_subpool` and `node_startup_state.l1_state.sl_chain_id` are both already in scope at
this point.

### Data flow

```
Node starts, starting_block == 1, protocol_version >= v31
  │
  ├─ upgrade_subpool     ← genesis upgrade tx (existing)
  └─ sl_chain_id_subpool ← SetSLChainId { chain_id: sl_chain_id, salt: u64::MAX }  (new)
  │
  └─ GatewayMigrationWatcher spawned concurrently, starts polling from L1 block 0

Block 1 execution
  └─ block executor includes both transactions

on_canonical_state_change (SlChainIdSubpool)
  └─ sees migration_number == u64::MAX → returns None (skips starting_migration_number update)
       └─ starting_migration_number remains 0

BlockContextProvider.on_canonical_state_change
  └─ outcome.last_migration_number == None → next_migration_number unchanged (stays 0)

On restart
  └─ GatewayMigrationWatcher: current_migration_number = 0 → binary search finds L1 block 0
```

### What is NOT changed

- `L1UpgradeTxWatcher` — no changes.
- `ISystemContext` in the contract interface — no getter added; no L2 RPC call needed.
- `GatewayMigrationWatcher` — no changes.
- `BlockContextProvider` — no changes; existing `None` guard already handles the sentinel path.
- `SystemTxEnvelope::set_sl_chain_id` — reused as-is with the sentinel salt.

## Invariants

- The bootstrap `SetSLChainId` is emitted exactly once: at genesis, when `starting_block == 1`
  and `protocol_version >= v31`.
- `starting_migration_number` is never set to `u64::MAX`; it remains `0` until the first real
  gateway migration fires.
- `sl_chain_id` at genesis is always the settlement layer chain ID (L1 chain ID when not on
  Gateway, gateway chain ID otherwise). It is never zero.
- The contract's own idempotency guard in `SystemContext.setSettlementLayerChainId`
  (`if (currentSettlementLayerChainId != _newSettlementLayerChainId)`) means a duplicate bootstrap
  call is a safe no-op on L2. This is an external assumption: the guard is present in
  `era-contracts/l1-contracts/contracts/l2-system/zksync-os/SystemContext.sol` and must remain
  for this invariant to hold.
