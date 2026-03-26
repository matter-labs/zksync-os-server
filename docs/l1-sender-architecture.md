# L1 Sender Component — Architecture Document

## Location

`lib/l1_sender/src/` — one Cargo crate, ~10 source files.

## Purpose

The L1 Sender is responsible for submitting ZKsync batch lifecycle transactions (**commit**, **prove**, **execute**) to the L1 (Ethereum) settlement layer. It runs as part of the **batcher subsystem** on the main node only. External nodes do not use it.

---

## File Map

| File | Role |
|---|---|
| `lib.rs` | Core async loop `run_l1_sender<Input: SendToL1>()` — receives commands, sends L1 txs, waits for inclusion, forwards downstream |
| `config.rs` | `L1SenderConfig<Input>` — operator signer, gas caps, command limit, Fusaka timestamp |
| `commands/mod.rs` | `SendToL1` trait + `L1SenderCommand<Command>` enum (`SendToL1` / `Passthrough`) |
| `commands/commit.rs` | `CommitCommand` — single batch, encodes `commitBatchesSharedBridge` or `commitBatchesMultisig`, provides blob sidecar |
| `commands/prove.rs` | `ProofCommand` — 1+ batches, encodes `proveBatchesSharedBridge`, computes SNARK public input |
| `commands/execute.rs` | `ExecuteCommand` — 1+ batches + priority ops + interop roots, encodes `executeBatchesSharedBridge` |
| `pipeline_component.rs` | `L1Sender<F, P, C>` — wraps `run_l1_sender` as a `PipelineComponent` |
| `upgrade_gatekeeper.rs` | `UpgradeGatekeeper` — blocks commit commands until L1 protocol version matches batch version |
| `batcher_model.rs` | Shared data types: `BatchMetadata`, `BatchEnvelope<E,S>`, `SignedBatchEnvelope`, `FriProof`, `SnarkProof`, `BatchSignatureData` |
| `batcher_metrics.rs` | `BatchExecutionStage` enum (21 stages) + `BatcherSubsystemMetrics` (shared across batcher subsystem) |
| `metrics.rs` | `L1SenderState` (4 states) + `L1SenderMetrics` (gas, fees, nonce, balance, blobs) |

---

## Core Abstraction: The `SendToL1` Trait

A single generic function `run_l1_sender<Input: SendToL1>()` handles all three L1 operations. The trait provides the operation-specific behavior:

```rust
trait SendToL1: Into<Vec<SignedBatchEnvelope<FriProof>>>
              + AsRef<[SignedBatchEnvelope<FriProof>]>
              + AsMut<[SignedBatchEnvelope<FriProof>]>
              + Display {
    const NAME: &'static str;                          // "commit" | "prove" | "execute"
    const SENT_STAGE: BatchExecutionStage;
    const MINED_STAGE: BatchExecutionStage;
    const PASSTHROUGH_STAGE: BatchExecutionStage;
    fn solidity_call(&self, gateway: bool, operator: &Address) -> Bytes;
    fn blob_sidecar(&self) -> Option<BlobTransactionSidecar> { None }
}
```

Three implementors: `CommitCommand`, `ProofCommand`, `ExecuteCommand`.

---

## Pipeline Position

The batcher pipeline is a linear chain of `PipelineComponent`s connected via tokio mpsc channels. The L1 senders sit in the second half:

```
Sequencer blocks
       │
       ▼
   Batcher (seals batches)
       │
       ▼
   BatchVerificationPipelineStep (signing)
       │
       ▼
   FriProvingPipelineStep (FRI proofs)
       │
       ▼
   GaplessCommitter (orders batches, creates CommitCommands, handles restart passthrough)
       │
       ▼
   UpgradeGatekeeper (blocks until L1 protocol version matches)
       │
       ▼
   L1Sender<CommitCommand>  ◄── sends commitBatches to L1
       │
       ▼
   SnarkProvingPipelineStep (SNARK proofs)
       │
       ▼
   GaplessL1ProofSender (orders out-of-order proofs)
       │
       ▼
   L1Sender<ProofCommand>   ◄── sends proveBatches to L1
       │
       ▼
   PriorityTreePipelineStep (builds priority ops and interop roots)
       │
       ▼
   L1Sender<ExecuteCommand>  ◄── sends executeBatches to L1
       │
       ▼
   BatchSink (persists final state)
```

All three `L1Sender` instances share the same config and target contract address (`validator_timelock_sl`), but each has its own provider (and therefore its own nonce sequence).

Wiring is in `node/bin/src/lib.rs` (~line 1044–1085), using `.pipe()` chaining.

---

## Main Loop (`run_l1_sender`)

The loop has four phases per iteration:

### Phase 1: Receive (`WaitingRecv`)

Blocks until at least 1 command arrives. Non-blocking drain of up to `command_limit` additional commands via `recv_many()`. After initial passthrough processing, only `SendToL1` variants are accepted — a `Passthrough` after any `SendToL1` is a hard error.

### Phase 2: Send to L1 (`SendingToL1`)

Iterates commands **sequentially** via `futures::stream::then()` (preserves nonce order). For each command:

1. Estimates EIP-1559 fees, caps at configured maximums
2. Builds `TransactionRequest` with gas fields, target contract, ABI-encoded calldata
3. If blob sidecar present: fetches blob base fee, optionally converts to EIP-7594 format post-Fusaka
4. Fills tx (nonce, gas estimate) via alloy provider
5. Sends raw transaction, registers receipt watcher with 1 confirmation / 300s timeout
6. Marks batch envelopes with `SENT_STAGE`

Note: transactions are **sent** sequentially (to preserve nonce ordering) but **waited for** in parallel.

### Phase 3: Wait for Inclusion (`WaitingL1Inclusion`)

Awaits all receipt futures **in order**. Validates each receipt (success check). On failure, calls `debug_trace_transaction` with call tracer to log revert reason, then crashes.

### Phase 4: Forward Downstream (`WaitingSend`)

Sends `SignedBatchEnvelope<FriProof>` to output channel in order, marking each with `MINED_STAGE`. Reports operator balance and nonce.

---

## Passthrough Mechanism

On startup after a restart, some batches may have already been committed/proved/executed on L1. The upstream `GaplessCommitter` (and analogous components for prove/execute) wraps these as `L1SenderCommand::Passthrough`.

Before entering the main loop, `process_prepending_passthrough_commands()` drains all leading passthrough commands using `peek_recv()` (non-consuming peek), forwarding them downstream with `PASSTHROUGH_STAGE`. Once a `SendToL1` command is peeked, normal processing begins. Any `Passthrough` after the first `SendToL1` is a hard error — this invariant is enforced in the main loop.

---

## Command Details

### CommitCommand (1 batch per L1 tx)

- Contains a single `SignedBatchEnvelope<FriProof>` + optional `BatchSignatureSet`
- `try_new()` validates signatures against L1's `BatchVerificationSL` config (allowed signers, threshold)
- Two contract call paths:
  - **With signatures**: `IMultisigCommitter::commitBatchesMultisig(chainAddress, batchFrom, batchTo, calldata, signers[], signatures[])`
  - **Without signatures**: `IExecutor::commitBatchesSharedBridge(chainAddress, batchFrom, batchTo, calldata)`
- Calldata encodes: previous `StoredBatchInfo`, new `CommitBatchInfo`, protocol version minor
- Only command type that provides a blob sidecar (EIP-4844 pubdata)
- Signatures are sorted by signer address before ABI encoding

### ProofCommand (1+ batches per L1 tx)

- Contains `Vec<SignedBatchEnvelope<FriProof>>` + `SnarkProof`
- Calls `IExecutor::proveBatchesSharedBridge(chainAddress, batchFrom, batchTo, proofPayload)`
- Proof payload structure:
  - Encoding version byte (`1`)
  - Previous `StoredBatchInfo`, new `StoredBatchInfo` array
  - Proof data (type + proof values)
- Computes SNARK public input: iterative `keccak256(prev_state || new_state || commitment)` with right-shift aggregation
- Proof types: `OHBENDER_PROOF_TYPE` (2, real) or `FAKE_PROOF_TYPE` (3, testing)
- Verifier version derived from `proving_execution_version` (supports versions 4, 5, 6)

### ExecuteCommand (1+ batches per L1 tx)

- Contains `Vec<SignedBatchEnvelope<FriProof>>` + `Vec<PriorityOpsBatchInfo>` + `Vec<Vec<InteropRoot>>`
- Calls `IExecutor::executeBatchesSharedBridge(chainAddress, batchFrom, batchTo, executePayload)`
- Execute payload varies by protocol version:
  - **v29–30**: `(storedBatchInfos, priorityOps, interopRoots)`
  - **v31–32**: adds `logs`, `messages`, `multichainRoots`, `operator` (only populated in gateway mode)
- Encoding version byte prefix (`1`)
- `assert_eq!(batches.len(), priority_ops.len())` enforced in constructor

---

## Gas Pricing and Transaction Management

### Configuration (`L1SenderConfig`)

```rust
struct L1SenderConfig<Input> {
    operator_signer: SignerConfig,       // Local private key or GCP KMS
    max_fee_per_gas_wei: u128,           // EIP-1559 cap
    max_priority_fee_per_gas_wei: u128,  // EIP-1559 priority cap
    max_fee_per_blob_gas_wei: u128,      // EIP-4844 blob gas cap
    command_limit: usize,                // Max commands per cycle
    poll_interval: Duration,             // (unused in main loop)
    fusaka_upgrade_timestamp: u64,       // When to switch to EIP-7594 blob format
}
```

### Gas Estimation Flow

1. Estimate EIP-1559 fees via `provider.estimate_eip1559_fees()`
2. Use **minimum** of estimated and configured values (prefer lower cost)
3. Warn if network suggests higher fees (may delay inclusion)
4. For blobs: fetch `provider.get_blob_base_fee()`, warn if above configured cap
5. Gas limit hardcoded to `15_000_000` (default from zksync-era)

### Nonce Management

- Provider fills nonce automatically via `provider.fill(tx_request)`
- Sequential sending within a cycle preserves nonce ordering
- **Critical invariant**: the same provider (sender address) must not be used outside this process; otherwise nonce conflicts occur
- On startup, operator balance is checked (zero balance = crash)

---

## UpgradeGatekeeper

Sits between `GaplessCommitter` and `L1Sender<CommitCommand>` in the pipeline.

- Peeks at each incoming `SendToL1` command's protocol version
- Polls L1 contract's `currentProtocolVersion` every 10 seconds
- Blocks until `L1 version == batch version`
- **Hard error** if `L1 version > batch version` (unexpected state)
- Passes `Passthrough` commands through without checking

---

## Data Model

### BatchMetadata

Core batch data flowing through the pipeline:

```rust
struct BatchMetadata {
    previous_stored_batch_info: StoredBatchInfo,
    batch_info: BatchInfo,             // commit info, chain address, etc.
    first_block_number: u64,
    last_block_number: u64,
    pubdata_mode: PubdataMode,
    tx_count: usize,
    execution_version: u32,
    protocol_version: ProtocolSemanticVersion,
    computational_native_used: Option<u64>,
    logs: Vec<L2Log>,                  // L2-to-L1 logs (for gateway mode)
    messages: Vec<Vec<u8>>,            // L2-to-L1 messages (for gateway mode)
    multichain_root: B256,             // Multichain root (for gateway mode)
}
```

Note: serialized in `ProofStorage`, so changes are breaking.

### BatchEnvelope / SignedBatchEnvelope

```rust
struct BatchEnvelope<E, S> {
    batch: BatchMetadata,
    data: E,                           // FriProof or other payload
    signature_data: S,                 // MissingSignature or BatchSignatureData
    latency_tracker: LatencyDistributionTracker<BatchExecutionStage>,
}

type SignedBatchEnvelope<E> = BatchEnvelope<E, BatchSignatureData>;
```

### FriProof (inner payload)

```rust
enum FriProof {
    Fake,                              // Testing
    AlreadySubmittedToL1,              // Batch already proven on L1
    Real(RealFriProof),                // Actual FRI proof bytes + execution version
}
```

### BatchSignatureData

```rust
enum BatchSignatureData {
    Signed { signatures: BatchSignatureSet },
    AlreadyCommitted,                  // Re-entering pipeline after restart
    NotNeeded,                         // Signing not enabled
}
```

---

## Metrics

### L1SenderState (reported per-sender)

| State | Meaning |
|---|---|
| `WaitingRecv` | Waiting for commands from upstream channel |
| `SendingToL1` | Building and sending L1 transactions |
| `WaitingL1Inclusion` | Waiting for L1 block inclusion (up to 300s) |
| `WaitingSend` | Waiting to forward envelopes downstream |

### Key L1SenderMetrics

- `l1_operator_address[operation, address]` — Gauge per signer (always 1)
- `balance[command]` — Operator wallet balance in ETH
- `parallel_transactions[command]` — Number of txs sent in one cycle
- `l1_transaction_fee_ether[command]` / `per_l2_tx` — Fee histograms
- `gas_used[command]` / `per_l2_tx` — Gas usage histograms
- `blob_base_fee_gwei`, `blob_gas_used` — EIP-4844 metrics
- `effective_gas_price_gwei[command]` — Post-execution gas price
- `nonce[command]` — Last nonce used
- `estimated_max_fee_per_gas_gwei` / `estimated_max_priority_fee_per_gas_gwei` — EIP-1559 estimates

### BatchExecutionStage (shared across batcher)

Tracks the lifecycle of each batch across the entire batcher pipeline:

```
BatchSealed → SigningStarted → BatchSigned → ProverInputStarted →
FriProverPicked → FriProvedReal/FriProvedFake → FriProofStored →
CommitL1TxSent → CommitL1TxMined (or CommitL1Passthrough) →
SnarkProverPicked → SnarkProvedReal/SnarkProvedFake →
ProveL1TxSent → ProveL1TxMined (or ProveL1Passthrough) →
ExecuteL1TxSent → ExecuteL1TxMined (or ExecuteL1Passthrough)
```

Each stage records latency (histogram) and current batch/block number (gauge).

---

## Error Handling

| Scenario | Behavior |
|---|---|
| L1 transaction timeout (300s) | Crash. Recovers on restart. |
| L1 transaction reverted | Log `debug_trace_transaction` output (revert reason). Crash. |
| Operator balance is zero | Crash on startup. |
| L1 protocol version > batch version | Hard crash (unexpected state). |
| L1 protocol version < batch version | Block and poll every 10s until they match. |
| Passthrough after SendToL1 | Hard error (invariant violation). |
| Nonce conflict (from prior in-flight tx) | Crash. Recovers on restart with correct nonce. |
| Network fee higher than configured cap | Warn, use configured cap anyway (may delay inclusion). |

**Recovery model**: crash-and-restart. The pipeline is designed to be replayed from the last known good state.

---

## External Dependencies

| Crate | Usage |
|---|---|
| `alloy` | Ethereum provider, wallet, transaction building, EIP-1559/4844/7594, ABI encoding |
| `zksync_os_contract_interface` | L1 contract ABIs (`IExecutor`, `IMultisigCommitter`), `StoredBatchInfo`, `PriorityOpsBatchInfo` |
| `zksync_os_operator_signer` | `SignerConfig` — local private key or GCP KMS key management |
| `zksync_os_pipeline` | `PipelineComponent` trait, `PeekableReceiver` |
| `zksync_os_observability` | `ComponentStateReporter`, `LatencyDistributionTracker` |
| `zksync_os_batch_types` | `BatchInfo`, `BatchSignatureSet` |
| `zksync_os_types` | `ProtocolSemanticVersion`, `PubdataMode`, `ProvingVersion` |
| `vise` | Prometheus metrics (histograms, gauges, counters) |

---

## Known Issues and TODOs

From code comments:

1. **No in-flight transaction detection on startup** — if a prior L1 tx gets mined after restart, the new tx with the same nonce will fail (recoverable on next restart).
2. **Crashes on L1 block gaps** — happens periodically with Infura provider.
3. **`batcher_model.rs` types are used across the entire batcher subsystem** — planned to be moved to a `batcher_types` crate.
4. **`BatchEnvelope` is almost always `BatchEnvelope<FriProof>`** — the generic parameter may not be justified.
5. **Fusaka EIP-7594 conversion is conditional** — waiting on anvil support (foundry issue #12222).
6. **Verifier version mapping is hardcoded** — `prove.rs` has a `todo: awful and temporary` comment on the match.
