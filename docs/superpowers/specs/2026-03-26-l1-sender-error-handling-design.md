# L1 Sender Error Handling Redesign

## Problem

The L1 sender crashes on any error. Every error — transient RPC timeout, gas spike, pending block gap — propagates as `Err` from `run_l1_sender`, hits `.expect("pipeline segment failed")` in `builder.rs:85`, panics the critical task, and kills the entire binary.

**Impact:**
- Kubernetes sees repeated crashes → CrashLoopBackOff (exponential restart delay up to 5min)
- Only alert fires on user impact ("no batches committed in 1.5h") — so up to 1.5h silent downtime
- Each restart risks double-sending (no in-flight tx detection)
- API, mempool, sequencer all die alongside the batcher

## Scope

**In scope:**
- Error categorization for all error paths in `run_l1_sender`
- Retry/backoff logic for transient and recoverable errors
- `GasBlocked` state with dedicated metric for early alerting
- Fix the `expect("no pending block")` Infura crash
- Raise `max_priority_fee_per_gas` default from 1 gwei to something effective
- Backpressure propagation (no internal buffering — let pipeline channels handle it)

**Out of scope:**
- Transaction replacement (re-submit at same nonce with higher gas) — follow-up
- Pre-flight `eth_call` simulation — follow-up
- In-flight transaction detection on startup — follow-up
- Decoupling sequencer from batcher pipeline — separate effort
- Database-backed transaction tracking (era model) — not planned

## Agreed Constraints

- When gas-blocked, the sender waits (doesn't crash, doesn't send doomed txs)
- Backpressure propagates naturally — no internal buffering
- The `GasBlocked` metric enables alerting within minutes (replaces the 1.5h user-impact threshold)
- Only truly fatal errors (data corruption, unsupported protocol version) should crash the binary

## Error Categories

Every error path in `run_l1_sender` falls into one of three categories.

**Note on startup errors:** `register_operator()` (signer registration, initial balance check) and `process_prepending_passthrough_commands()` run before the main loop. These remain Fatal — if the operator can't be registered, the sender can't function. The retry logic only applies to the main send loop.

### Transient — retry with backoff

Temporary infrastructure issues. Will resolve on their own.

| Error path | Code location | Notes |
|---|---|---|
| `provider.estimate_eip1559_fees().await?` | `lib.rs:288` | RPC timeout/rate limit |
| `provider.get_blob_base_fee().await?` | `lib.rs:147` | RPC timeout/rate limit |
| `provider.fill(tx_request).await?` (RPC portion) | `lib.rs:165` | Nonce fetch or gas estimation RPC failure |
| `provider.get_block(BlockId::pending()).await?` | `lib.rs:167` | RPC timeout; `None` result falls back to latest block |
| `provider.send_raw_transaction().await?` (RPC timeout) | `lib.rs:188` | Network-level failure, not mempool rejection |
| `provider.get_balance().await?` | `lib.rs:219` | Informational — failure should be logged and ignored, not propagated |
| `provider.get_transaction_count().await?` | `lib.rs:220` | Informational — same as above |
| Receipt polling RPC failures | `lib.rs:214` | Alloy watcher RPC errors during polling |

**Behavior:** Log warning, sleep with exponential backoff (5s → 10s → 20s → 40s → 60s, capped at 60s), retry. Reset backoff on success. After 30 consecutive transient failures (~15 min at cap), emit a `persistent_transient_errors` metric for alerting.

**Important:** `try_into_envelope()` and `try_into_pooled()` at `lib.rs:165` are local conversion errors (not RPC). If these fail, it indicates a code bug or data corruption — classify as **Fatal**, not Transient.

### Recoverable — wait for external condition to change

Not a bug, but the sender can't proceed right now.

| Error path | Code location | Condition |
|---|---|---|
| Gas fees above configured cap | `lib.rs:296-306` | Network congestion |
| Blob fees above configured cap | `lib.rs:152-158` | Blob demand spike (current code warns and sends anyway — new behavior: block) |
| Tx timeout (300s) | `lib.rs:194-198` | Tx stuck in mempool |
| Nonce too low (mempool rejection) | `lib.rs:188` | Prior tx mined; requires parsing RPC error code/message to detect |
| Low operator balance | (new check) | Balance dropped below threshold during operation |

**Behavior:** Enter specific state (`GasBlocked`, `BlobFeeBlocked`, `WaitingForInclusion`), emit dedicated metric per state, sleep 30-60s, re-check condition. Do not crash.

**Note on `outbound.send().await?`:** `mpsc::Sender::send().await` blocks until the channel has capacity — it never errors on "channel full." The only error is receiver-dropped (downstream crashed), which is **Fatal**. This was incorrectly listed as Recoverable in an earlier draft.

**Note on nonce detection:** `send_raw_transaction` returns a generic transport error when the mempool rejects a tx. To distinguish "nonce too low" from other rejections, we need to pattern-match on the RPC error message or code (e.g., "nonce too low", code -32000 on most clients). This parsing should be best-effort — if the error doesn't match a known pattern, treat it as Transient.

### Fatal — crash (correct behavior)

Unrecoverable without code or infrastructure changes.

| Error path | Code location | Reason |
|---|---|---|
| Zero operator balance at startup | `lib.rs:346` | Needs manual funding |
| Signer registration failure | `lib.rs:338` | KMS/key config broken |
| Tx reverted on L1 | `lib.rs:368-399` | Contract/calldata bug, already burned gas |
| Unsupported protocol version | `execute.rs:146-149`, `prove.rs:148` | Needs code update |
| Invalid blob data / `try_into_eip7594` | `lib.rs:172-178` | Data corruption |
| `try_into_envelope()` / `try_into_pooled()` failure | `lib.rs:165` | Malformed tx data |
| Corrupted proof bytes | `prove.rs:176` | Data corruption |
| Passthrough after SendToL1 | `lib.rs:111-115` | Pipeline protocol violation |
| Inbound channel closed unexpectedly | `lib.rs:121` | Upstream crashed |
| Outbound channel closed (receiver dropped) | `lib.rs:234` | Downstream crashed |

**Behavior:** Log error with full context, return `Err` (crashes binary via pipeline `.expect()`). This is correct — manual intervention needed.

### Non-propagating errors (log and ignore)

These currently propagate via `?` but should never crash the binary.

| Error path | Code location | Reason |
|---|---|---|
| `L1_SENDER_METRICS.report_tx_receipt()?` | `metrics.rs:104-149` | `.parse::<f64>()` on formatted ether — metrics formatting failure |
| `L1_SENDER_METRICS.report_l1_eip_1559_estimation()?` | `metrics.rs:150-158` | Same |
| `L1_SENDER_METRICS.report_blob_base_fee()?` | `metrics.rs:160-163` | Same |
| `format_ether(balance).parse()?` | `lib.rs:228` | Informational balance reporting |

**Behavior:** Log the error at `warn` level, continue. A metrics formatting failure should never affect the send loop.

---

# Approach A: Retry Wrapper

## Summary

Keep `run_l1_sender` as a single function. Extract the loop body into `try_send_cycle()`, wrap it in a match on a new error enum. Minimal structural change.

## New Types

```rust
/// Categorized error from the L1 sender's send cycle.
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

## Changes to `lib.rs`

### 1. New states for metrics

Add to `L1SenderState`:

```rust
enum L1SenderState {
    WaitingRecv,
    WaitingSend,
    SendingToL1,
    WaitingL1Inclusion,
    GasBlocked,          // NEW — network gas fees above configured cap
    BlobFeeBlocked,      // NEW — blob fees above configured cap
    TransientBackoff,    // NEW — retrying after transient RPC error
}
```

### 2. Main loop becomes retry-aware (with held commands)

```rust
pub async fn run_l1_sender<Input: SendToL1>(...) -> anyhow::Result<()> {
    // ... register_operator, process passthrough (unchanged) ...

    let mut backoff = ExponentialBackoff::new(5s, 60s);
    let mut held_commands: Option<Vec<Input>> = None;

    loop {
        // Re-use held commands from a failed cycle, or consume new ones
        let commands = if let Some(cmds) = held_commands.take() {
            cmds
        } else {
            latency_tracker.enter_state(L1SenderState::WaitingRecv);
            let received = inbound.recv_many(&mut cmd_buffer, config.command_limit).await;
            if received == 0 { return Ok(()); }
            // Drain + validate (unchanged)
            /* ... same drain + collect logic ... */
        };

        match try_send_cycle(&commands, &provider, &config, &outbound, ...).await {
            Ok(()) => {
                backoff.reset();
            }
            Err(L1SendError::Transient(e)) => {
                tracing::warn!(?e, "transient error, retrying");
                latency_tracker.enter_state(L1SenderState::TransientBackoff);
                metrics.transient_errors.inc();
                held_commands = Some(commands);  // Retain for retry
                tokio::time::sleep(backoff.next()).await;
                continue;
            }
            Err(L1SendError::Recoverable { reason, source }) => {
                tracing::warn!(?source, ?reason, "recoverable error, waiting");
                latency_tracker.enter_state(match reason {
                    RecoverableReason::GasBlocked => L1SenderState::GasBlocked,
                    RecoverableReason::BlobFeeBlocked => L1SenderState::BlobFeeBlocked,
                    _ => L1SenderState::TransientBackoff,
                });
                metrics.recoverable_errors.inc();
                held_commands = Some(commands);  // Retain for retry
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
            Err(L1SendError::Fatal(e)) => {
                tracing::error!(?e, "fatal error");
                return Err(e);
            }
        }
    }
}
```

### 3. `try_send_cycle` is today's loop body with categorized errors

Each `?` becomes an explicit match that returns the appropriate `L1SendError` variant. For example:

```rust
// Before:
let eip1559_est = provider.estimate_eip1559_fees().await?;

// After:
let eip1559_est = provider.estimate_eip1559_fees().await
    .map_err(|e| L1SendError::Transient(e.into()))?;
```

And for gas cap checks:

```rust
// Before: warn and continue with capped value
// After: return Recoverable if ALL fee components exceed caps
if eip1559_est.max_fee_per_gas > max_fee_per_gas {
    return Err(L1SendError::Recoverable {
        reason: RecoverableReason::GasBlocked,
        source: anyhow!("network fee {} exceeds cap {}", ...),
    });
}
```

### 4. Fix pending block panic

```rust
// Before:
provider.get_block(BlockId::pending()).await?.expect("no pending block");

// After:
let block = provider.get_block(BlockId::pending()).await
    .map_err(|e| L1SendError::Transient(e.into()))?;
let block = match block {
    Some(b) => b,
    None => provider.get_block(BlockId::latest()).await
        .map_err(|e| L1SendError::Transient(e.into()))?
        .expect("no latest block"),
};
```

### 5. Fee defaults change

In `node/bin/src/config/mod.rs`:

```rust
// Before:
#[config(default_t = 1 * EtherUnit::Gwei)]
pub max_priority_fee_per_gas: EtherAmount,

// After:
#[config(default_t = 10 * EtherUnit::Gwei)]
pub max_priority_fee_per_gas: EtherAmount,
```

## Critical Constraint: Commands Must Not Be Lost

Upstream components (GaplessCommitter, GaplessL1ProofSender, PriorityTreePipelineStep) are **fire-and-forget**: once a command is sent to the channel, they do not re-produce it. If the L1 sender consumes a command from the channel and then discards it after an error, that batch is **permanently lost** until a full binary restart.

This means the naive version of Approach A (discard commands on error) is unsafe. To make Approach A viable, consumed commands must be retained across retries:

```rust
// Commands are drained ONCE and held across retry iterations
let mut held_commands: Option<Vec<Input>> = None;

loop {
    let commands = if let Some(cmds) = held_commands.take() {
        cmds  // Re-use commands from a failed cycle
    } else {
        // Only consume from channel if we have no held commands
        inbound.recv_many(&mut cmd_buffer, config.command_limit).await;
        /* ... drain + validate ... */
    };

    match try_send_cycle(&commands, ...).await {
        Ok(()) => { backoff.reset(); }
        Err(L1SendError::Transient(_)) | Err(L1SendError::Recoverable { .. }) => {
            held_commands = Some(commands);  // Hold for retry
            sleep(backoff.next()).await;
            continue;
        }
        Err(L1SendError::Fatal(e)) => return Err(e),
    }
}
```

This adds a small amount of state (one `Option<Vec>`) but keeps the overall shape of Approach A intact.

## Limitations of Approach A (with held commands fix)

1. **No partial progress.** If 3 of 5 txs were sent before the error, those 3 are in-flight with no tracking. On retry, the cycle re-sends all 5 — but the nonces for the first 3 are already consumed. This causes nonce conflicts (recoverable via retry, but wastes cycles). The held commands don't know which ones were already sent.

2. **No tx replacement.** A timed-out tx stays in the mempool. The retry re-estimates fees and tries a new tx, but with a new nonce (the old one is taken). If gas drops and the old tx gets mined, the new one's nonce is now wrong. This can lead to double-execution of a batch (which would revert on L1 and become Fatal).

3. **Gas check is all-or-nothing.** If fees are above the cap, we don't send at all. If fees are below the cap, we send everything. There's no escalation within a cycle.

4. **Receipt futures are consumed.** After a TxTimeout, the `BoxFuture` is gone — we can't re-poll. The retry starts a completely new send cycle, re-submitting the same commands (with new nonces), which means the timed-out tx and the new tx can both land on L1.

## Files Changed

| File | Change |
|---|---|
| `lib/l1_sender/src/lib.rs` | Extract `try_send_cycle`, add retry loop, fix pending block |
| `lib/l1_sender/src/metrics.rs` | Add `GasBlocked`, `TransientBackoff` states, error counters |
| `node/bin/src/config/mod.rs` | Raise `max_priority_fee_per_gas` default to 10 gwei |

Estimated: ~200 lines added/changed, 3 files.

---

# Approach B: Phase-Based Struct

## Summary

Extract `run_l1_sender` into a `L1SenderLoop` struct with explicit phase methods: `receive()`, `send_pending()`, `wait_for_inclusion()`, `forward_downstream()`. Each phase returns typed errors. The struct holds intermediate state so partial progress survives errors.

## New Types

```rust
/// Same error enum as Approach A.
enum L1SendError {
    Transient(anyhow::Error),
    Recoverable { reason: RecoverableReason, source: anyhow::Error },
    Fatal(anyhow::Error),
}

enum RecoverableReason {
    GasBlocked,
    BlobFeeBlocked,
    TxTimeout,
    NonceTooLow,
}

/// Tracks a transaction that has been submitted to L1 but not yet confirmed.
struct InFlightTx<Input> {
    command: Input,
    receipt_future: TransactionReceiptFuture,
}

/// The gas parameters used for a transaction, for metrics/logging.
struct GasParams {
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    max_fee_per_blob_gas: Option<u128>,
}
```

## The Struct

```rust
struct L1SenderLoop<Input: SendToL1, F, P> {
    // Plumbing (moved from function args)
    inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: Sender<SignedBatchEnvelope<FriProof>>,
    provider: FillProvider<F, P>,
    config: L1SenderConfig<Input>,
    to_address: Address,
    operator_address: Address,
    gateway: bool,

    // State that survives errors
    pending_commands: Vec<Input>,           // consumed from channel, not yet sent
    in_flight: Vec<InFlightTx<Input>>,      // sent to L1, waiting receipt
    completed: Vec<Input>,                  // receipt received, not yet forwarded

    // Observability
    latency_tracker: ComponentStateHandle<L1SenderState>,
    backoff: ExponentialBackoff,
}
```

## Phase Methods

### `receive()` — fills `pending_commands`

```rust
/// Reads commands from the inbound channel into pending_commands.
/// Only called when pending_commands is empty.
/// Returns Fatal if channel is closed.
async fn receive(&mut self) -> Result<(), L1SendError> {
    let received = self.inbound
        .recv_many(&mut self.cmd_buffer, self.config.command_limit)
        .await;
    if received == 0 {
        return Err(L1SendError::Fatal(anyhow!("inbound channel closed")));
    }
    // Drain into pending_commands (same validation as today)
    self.pending_commands = self.cmd_buffer.drain(..)
        .map(|cmd| match cmd {
            L1SenderCommand::SendToL1(c) => Ok(c),
            L1SenderCommand::Passthrough(_) => Err(L1SendError::Fatal(...)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(())
}
```

### `send_pending()` — moves `pending_commands` → `in_flight`

```rust
/// Sends each pending command to L1. On success, moves to in_flight.
/// On transient error, stops sending — remaining commands stay in pending_commands.
async fn send_pending(&mut self) -> Result<(), L1SendError> {
    // Check gas fees BEFORE sending anything
    let gas = self.estimate_fees().await?;  // Transient if RPC fails
    if gas.max_fee_per_gas > self.config.max_fee_per_gas_wei {
        return Err(L1SendError::Recoverable {
            reason: RecoverableReason::GasBlocked, ...
        });
    }

    // Send one at a time — partial progress preserved
    while let Some(cmd) = self.pending_commands.first() {
        match self.send_one_tx(cmd, &gas).await {
            Ok(receipt_future) => {
                let cmd = self.pending_commands.remove(0);
                self.in_flight.push(InFlightTx { command: cmd, receipt_future });
            }
            Err(L1SendError::Transient(e)) => {
                // Stop sending, but don't lose remaining commands
                tracing::warn!(?e, remaining = self.pending_commands.len(),
                    "transient error during send, will retry remaining");
                return Err(L1SendError::Transient(e));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
```

### `wait_for_inclusion()` — moves `in_flight` → `completed`

```rust
/// Waits for all in-flight txs to be included. On timeout, returns Recoverable.
/// Successfully included txs move to completed even if later ones fail.
async fn wait_for_inclusion(&mut self) -> Result<(), L1SendError> {
    while let Some(tx) = self.in_flight.first_mut() {
        match (&mut tx.receipt_future).await {
            Ok(receipt) => {
                if receipt.status() {
                    self.report_receipt_metrics(&tx.command, &receipt)?;
                    let tx = self.in_flight.remove(0);
                    self.completed.push(tx.command);
                } else {
                    // TX reverted on L1 — fatal, gas already burned
                    self.log_revert_trace(&receipt).await;
                    return Err(L1SendError::Fatal(anyhow!("tx reverted on L1")));
                }
            }
            Err(PendingTransactionError::Timeout) => {
                return Err(L1SendError::Recoverable {
                    reason: RecoverableReason::TxTimeout, ...
                });
            }
            Err(e) => {
                return Err(L1SendError::Transient(e.into()));
            }
        }
    }
    Ok(())
}
```

### `forward_downstream()` — drains `completed`

```rust
/// Sends completed commands downstream. Fails only if channel is closed.
async fn forward_downstream(&mut self) -> Result<(), L1SendError> {
    for command in self.completed.drain(..) {
        for mut envelope in command.into() {
            envelope.set_stage(Input::MINED_STAGE);
            self.outbound.send(envelope).await
                .map_err(|e| L1SendError::Fatal(e.into()))?;
        }
    }
    Ok(())
}
```

## Main Loop

```rust
pub async fn run(mut self) -> anyhow::Result<()> {
    self.process_prepending_passthroughs().await?;

    loop {
        // Phase 1: receive (only if nothing pending)
        if self.pending_commands.is_empty() && self.in_flight.is_empty() && self.completed.is_empty() {
            match self.receive().await {
                Ok(()) => {}
                Err(L1SendError::Fatal(e)) => return Err(e),
                Err(_) => unreachable!("receive only returns Fatal"),
            }
        }

        // Phase 2: send pending commands
        if !self.pending_commands.is_empty() {
            match self.send_pending().await {
                Ok(()) => {}
                Err(L1SendError::Transient(e)) => {
                    self.enter_backoff(L1SenderState::TransientBackoff).await;
                    continue;
                }
                Err(L1SendError::Recoverable { reason, .. }) => {
                    self.enter_state_and_wait(reason).await;
                    continue;
                }
                Err(L1SendError::Fatal(e)) => return Err(e),
            }
        }

        // Phase 3: wait for in-flight txs
        if !self.in_flight.is_empty() {
            match self.wait_for_inclusion().await {
                Ok(()) => {}
                Err(L1SendError::Transient(e)) => {
                    // Receipt polling failed — in_flight still tracked, will re-poll
                    self.enter_backoff(L1SenderState::TransientBackoff).await;
                    continue;
                }
                Err(L1SendError::Recoverable { reason: RecoverableReason::TxTimeout, .. }) => {
                    // TX stuck — for now, just wait and re-poll.
                    // Future: tx replacement at same nonce.
                    tracing::warn!("tx timed out, will re-poll");
                    self.enter_backoff(L1SenderState::WaitingL1Inclusion).await;
                    continue;
                }
                Err(L1SendError::Fatal(e)) => return Err(e),
                Err(e) => return Err(e.into_anyhow()),
            }
        }

        // Phase 4: forward completed downstream
        if !self.completed.is_empty() {
            self.forward_downstream().await?;
        }

        self.report_balance_and_nonce().await;
        self.backoff.reset();
    }
}
```

## What This Enables (Now)

1. **Partial send progress.** If 3 of 5 txs are sent and then RPC fails, the 3 are in `in_flight` and the remaining 2 stay in `pending_commands`. On retry, only the 2 remaining are sent.

2. **No lost commands.** Commands consumed from the channel live in `pending_commands` until successfully sent. No data loss on transient errors.

3. **Timeout doesn't crash.** A stuck tx stays in `in_flight`. The sender can re-poll the receipt, wait longer, or (in a follow-up) replace the tx.

4. **Gas-blocked state is clean.** Before sending anything, fees are checked. If above cap, we enter `GasBlocked` state. No txs are submitted, no gas is wasted, the metric fires immediately.

## What This Enables (Follow-ups)

The struct's `in_flight` tracking makes these future improvements straightforward:

- **Tx replacement:** When a tx times out, we have its nonce. Submit a replacement at the same nonce with bumped fees. Requires explicit nonce management (not auto-fill).
- **Pre-flight simulation:** Add `eth_call` in `send_one_tx` before `send_raw_transaction`.
- **In-flight detection on startup:** Query pending nonces from L1, compare with expected, detect already-sent txs.

## Limitations of Approach B

1. **Larger diff.** ~300-400 lines added/changed vs ~150 for Approach A.
2. **State management complexity.** Three collections (`pending`, `in_flight`, `completed`) must stay consistent. A bug in transitions could cause subtle issues (e.g., a command stuck in `in_flight` forever).
3. **Receipt futures are not re-pollable.** Once a `BoxFuture` is polled to completion (timeout), it's consumed. If we want to re-poll after a TxTimeout, we'd need to re-register the watcher. This is solvable but adds implementation complexity.
4. **Nonce management stays implicit for now.** We still use `provider.fill()` for nonce assignment. Tx replacement (which requires explicit nonces) is deferred to a follow-up.

## Files Changed

| File | Change |
|---|---|
| `lib/l1_sender/src/lib.rs` | Replace `run_l1_sender` with `L1SenderLoop` struct + phase methods |
| `lib/l1_sender/src/error.rs` | New file: `L1SendError`, `RecoverableReason` |
| `lib/l1_sender/src/metrics.rs` | Add `GasBlocked`, `TransientBackoff` states, error counters |
| `lib/l1_sender/src/pipeline_component.rs` | Update to construct and run `L1SenderLoop` |
| `node/bin/src/config/mod.rs` | Raise `max_priority_fee_per_gas` default to 10 gwei |

Estimated: ~400 lines added/changed, 5 files.

---

# Comparison

| Dimension | Approach A (with held commands) | Approach B |
|---|---|---|
| Diff size | ~200 lines, 3 files | ~400 lines, 5 files |
| Commands lost on error | No (held in `Option<Vec>`) | No (held in struct fields) |
| Partial send progress | No — whole cycle retries (nonce conflicts possible) | Yes — only unsent commands retry |
| Tx timeout handling | Re-send all commands (risk of double-send) | Re-poll or wait (tx stays tracked) |
| Gas-blocked state | Yes | Yes |
| Dedicated metrics | Yes | Yes |
| Foundation for tx replacement | No | Yes (`in_flight` tracking) |
| Foundation for pre-flight sim | Possible but awkward | Natural (add to `send_one_tx`) |
| Risk of state bugs | Low (one `Option<Vec>`) | Medium (3 collections with transitions) |
| Testability | Same as today (hard) | Better (methods testable independently) |
| Double-send risk after timeout | Yes — timed-out tx + retry tx can both land | Lower — in-flight tracking enables future replacement |

## Recommendation

**Approach A** stops the binary from crashing and is quick to ship. But it has a real double-send risk after tx timeouts: the old tx may still be in the mempool when the retry sends a new tx at a different nonce. Both can land on L1 — the second would revert (wasting gas) or, in the worst case, execute the same batch twice (which the L1 contract should reject). This risk exists in the current code too (crash-restart has the same problem), but Approach A makes it more frequent by retrying in-process instead of restarting.

**Approach B** costs ~200 more lines but eliminates the double-send risk by tracking in-flight txs. It also provides the foundation for tx replacement (the most impactful follow-up). The state management is more complex, but each phase is independently testable and the state transitions are straightforward (pending → in_flight → completed is a linear pipeline within the sender).

Both approaches share the same error enum, metric additions, and fee default change — so starting with A and migrating to B later is feasible but involves re-touching the same code twice.

# Testing Strategy

Both approaches need tests validating the core invariants:

1. **Transient errors don't crash.** Mock provider returns RPC errors for N calls, then succeeds. Verify the sender retries and eventually succeeds without returning `Err`.
2. **Commands are not lost.** Inject a transient error mid-cycle. Verify all consumed commands are eventually sent or remain available for retry.
3. **Gas-blocked state emits metrics.** Configure a low gas cap, mock provider returns high fees. Verify `GasBlocked` state is entered and the metric is emitted.
4. **Fatal errors still crash.** Mock a tx revert receipt. Verify the sender returns `Err`.
5. **Backoff resets on success.** Verify that after a successful cycle, the backoff duration returns to the initial value.

For Approach B specifically:
6. **Partial progress.** Send 3 of 5 txs, inject RPC error. Verify only 2 remain in `pending_commands` and the 3 are in `in_flight`.
7. **State transitions are consistent.** After a full cycle, all three collections (`pending`, `in_flight`, `completed`) are empty.
