# Implementation Prompt: L1 Sender Error Handling Redesign (Approach B)

## Task

Redesign the L1 sender in `lib/l1_sender/src/` to categorize all errors and handle them appropriately instead of crashing the binary. Implement "Approach B: Phase-Based Struct" from the design spec.

## Context

Read these files first — they contain the full architecture, comparison with zksync-era, flaw analysis, and design spec:

1. **Design spec (your primary guide):** `docs/superpowers/specs/2026-03-26-l1-sender-error-handling-design.md` — read the "Approach B" section and the "Error Categories" section completely
2. **Architecture doc:** `docs/l1-sender-architecture.md` — understand the pipeline position and data flow
3. **Flaws doc:** `docs/l1-sender-flaws.md` — context on why this redesign is needed
4. **Investigation doc:** `docs/l1-sender-investigation.md` — detailed analysis of each issue

Then read the source files you'll be modifying:

5. `lib/l1_sender/src/lib.rs` — the main `run_l1_sender` function (~240 lines, the core of this change)
6. `lib/l1_sender/src/config.rs` — `L1SenderConfig`
7. `lib/l1_sender/src/commands/mod.rs` — `SendToL1` trait, `L1SenderCommand` enum
8. `lib/l1_sender/src/pipeline_component.rs` — `L1Sender` pipeline wrapper
9. `lib/l1_sender/src/metrics.rs` — `L1SenderState`, `L1SenderMetrics`
10. `lib/l1_sender/src/batcher_metrics.rs` — `BatchExecutionStage`
11. `lib/l1_sender/src/batcher_model.rs` — `SignedBatchEnvelope`, `FriProof`
12. `node/bin/src/config/mod.rs` — node-level config (search for `max_priority_fee_per_gas`)
13. `node/bin/src/provider.rs` — how the alloy provider is built (retry policy)
14. `lib/pipeline/src/builder.rs` — how pipeline errors crash the binary (line 85)

## What to Implement

### 1. Create `lib/l1_sender/src/error.rs`

New file with:

```rust
enum L1SendError {
    /// RPC down, timeout, rate limit. Retry with backoff.
    Transient(anyhow::Error),
    /// Gas too high, tx stuck, nonce conflict. Wait and retry.
    Recoverable { reason: RecoverableReason, source: anyhow::Error },
    /// Unrecoverable. Crash the binary.
    Fatal(anyhow::Error),
}

enum RecoverableReason {
    GasBlocked,
    BlobFeeBlocked,
    TxTimeout,
    NonceTooLow,
}
```

Add a helper `L1SendError::into_anyhow(self) -> anyhow::Error` for the Fatal variant.

Add a helper to classify `send_raw_transaction` RPC errors: if the error message contains "nonce too low" (or similar patterns from common clients), return `Recoverable::NonceTooLow`; otherwise return `Transient`.

### 2. Add new states to `L1SenderState` in `metrics.rs`

```rust
enum L1SenderState {
    WaitingRecv,
    WaitingSend,
    SendingToL1,
    WaitingL1Inclusion,
    GasBlocked,          // NEW
    BlobFeeBlocked,      // NEW
    TransientBackoff,    // NEW
}
```

Add error counter metrics:
- `transient_errors: Counter` — incremented on each transient error
- `recoverable_errors: LabeledFamily<&'static str, Counter>` — labeled by `RecoverableReason`

### 3. Replace `run_l1_sender` with `L1SenderLoop` struct

This is the main change. Convert the monolithic async function into a struct with phase methods.

**Struct fields:**
- All current function parameters (inbound, outbound, provider, config, to_address, gateway)
- `operator_address: Address` (from `register_operator`)
- `pending_commands: Vec<Input>` — consumed from channel, not yet sent
- `in_flight: Vec<InFlightTx<Input>>` — sent to L1, awaiting receipt
- `completed: Vec<Input>` — receipt received, not yet forwarded downstream
- `latency_tracker: ComponentStateHandle<L1SenderState>`
- `backoff: ExponentialBackoff` (implement a simple one: initial 5s, max 60s, multiply by 2)
- `cmd_buffer: Vec<L1SenderCommand<Input>>` — reusable buffer for `recv_many`

**`InFlightTx` struct:**
```rust
struct InFlightTx<Input> {
    command: Input,
    receipt_future: TransactionReceiptFuture,
}
```

**Phase methods** (see design spec for pseudocode):
- `async fn receive(&mut self) -> Result<(), L1SendError>` — reads from inbound channel into `pending_commands`
- `async fn send_pending(&mut self) -> Result<(), L1SendError>` — estimates fees, sends one tx at a time, moves to `in_flight`
- `async fn wait_for_inclusion(&mut self) -> Result<(), L1SendError>` — awaits receipt futures in order, moves to `completed`
- `async fn forward_downstream(&mut self) -> Result<(), L1SendError>` — sends completed envelopes to outbound

**Main loop:**
- Only call `receive()` when all three collections are empty
- Each phase checks its precondition (`!self.pending_commands.is_empty()`, etc.)
- Transient errors → enter backoff, continue loop
- Recoverable errors → enter specific state (GasBlocked etc.), sleep 30s, continue
- Fatal errors → return Err (crashes binary, which is correct)
- Reset backoff after a fully successful cycle

**Key behaviors:**
- **Gas check happens in `send_pending()` BEFORE sending any tx.** If fees exceed the cap, return `Recoverable::GasBlocked`. No tx is submitted, no gas is wasted.
- **Blob fee check:** Same — if blob base fee exceeds cap, return `Recoverable::BlobFeeBlocked` instead of sending a doomed tx (this changes current behavior which warns and sends anyway).
- **Partial progress in `send_pending()`:** Send commands one at a time. If the 3rd of 5 fails, the first 2 are in `in_flight` and the remaining 3 stay in `pending_commands`.
- **Tx timeout in `wait_for_inclusion()`:** When `PendingTransactionError::Timeout` occurs, return `Recoverable::TxTimeout`. The timed-out tx stays in `in_flight` — but the receipt future is consumed (BoxFuture). On the next iteration, we need to re-register a watcher for this tx. Since we don't have the tx hash stored separately, the simplest approach for now is to log a warning and continue waiting (increase the timeout or re-create the watcher). See the design spec's "Limitations" section.

### 4. Fix the pending block panic

In the send phase, replace:
```rust
provider.get_block(BlockId::pending()).await?.expect("no pending block")
```
with a fallback to `BlockId::latest()` if pending returns `None`. This fixes the known Infura crash.

### 5. Make metrics errors non-propagating

In `metrics.rs`, change `report_tx_receipt`, `report_l1_eip_1559_estimation`, and `report_blob_base_fee` to not return `Result` (or log errors internally). These `.parse::<f64>()` calls should never crash the binary.

Similarly, the informational `get_balance` and `get_transaction_count` calls after successful inclusion (currently at `lib.rs:219-220`) should be wrapped in error handling — log a warning on failure, don't propagate.

### 6. Update `pipeline_component.rs`

The `L1Sender` pipeline component should construct an `L1SenderLoop` and call its `run()` method.

### 7. Raise the priority fee default

In `node/bin/src/config/mod.rs`, change:
```rust
#[config(default_t = 1 * EtherUnit::Gwei)]
pub max_priority_fee_per_gas: EtherAmount,
```
to:
```rust
#[config(default_t = 10 * EtherUnit::Gwei)]
pub max_priority_fee_per_gas: EtherAmount,
```

### 8. Update `lib.rs` module declarations

Add `mod error;` to `lib.rs` and re-export the error types.

## Critical Gotchas

1. **Commands must NEVER be lost.** Upstream components (GaplessCommitter, GaplessL1ProofSender, PriorityTreePipelineStep) are fire-and-forget. Once a command is consumed from the channel, it must be held in the struct until successfully forwarded downstream.

2. **`outbound.send().await` never errors on "channel full"** — it blocks. The only error is receiver-dropped (Fatal).

3. **Receipt futures (`BoxFuture`) are consumed on poll.** After a timeout, you can't re-poll the same future. For the TxTimeout case, the simplest approach is to log a warning and attempt to re-create a receipt watcher using the tx hash (if available) or accept that this edge case requires a process restart for now. Document this as a known limitation.

4. **`try_into_envelope()` and `try_into_pooled()` are local conversion errors, not RPC errors.** Classify as Fatal, not Transient.

5. **Nonce management stays implicit** (via `provider.fill()`). Do NOT switch to explicit nonce management in this PR — that's a follow-up for tx replacement.

6. **The `SendToL1` trait requires `Into<Vec<SignedBatchEnvelope<FriProof>>>`.** This consumes the command. The `forward_downstream` phase calls `.into()` on completed commands, so commands can only be forwarded once. The `completed` vec should be drained, not iterated.

7. **The passthrough handling (`process_prepending_passthrough_commands`) stays as-is** — it runs before the main loop and its errors remain Fatal.

8. **All three L1 senders (commit, prove, execute) use the same generic code.** The `SendToL1` trait parameterizes behavior. Your `L1SenderLoop` should be generic over `Input: SendToL1` just like the current `run_l1_sender`.

## Pre-submission Checklist

Run all of these before pushing:

1. `cargo fmt --all --check`
2. `cargo clippy --all-targets --all-features --workspace -- -D warnings`
3. `cargo nextest run --release --workspace --exclude zksync_os_integration_tests`
4. `cargo nextest run -p zksync_os_integration_tests`

## Estimated Scope

~400 lines changed across 5 files:
- `lib/l1_sender/src/lib.rs` — major rewrite (function → struct + methods)
- `lib/l1_sender/src/error.rs` — new file
- `lib/l1_sender/src/metrics.rs` — new states + error counters
- `lib/l1_sender/src/pipeline_component.rs` — adapt to new struct
- `node/bin/src/config/mod.rs` — priority fee default change
