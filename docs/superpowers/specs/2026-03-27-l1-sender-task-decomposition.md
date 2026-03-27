# L1 Sender: Task Decomposition Design

## Problem

The current `L1SenderLoop` has four sequential phases inside a single loop:

1. **Receive** — pull commands from the upstream channel
2. **send_pending** — submit each command as an L1 tx
3. **wait_for_inclusion** — poll receipt futures one at a time
4. **forward_downstream** — send confirmed commands downstream

Any non-fatal error in phase N causes a `continue` that skips phases N+1 through 4. This
creates two concrete blocking problems:

**Phase 2 error blocks phase 3.**
If 4 of 5 pending commands are submitted successfully before a transient RPC failure, the
4 in-flight transactions do not get their receipts checked until the 5th command is
successfully sent on the next iteration.

**Phase 3 error blocks phase 4.**
If 4 of 5 in-flight transactions have confirmed receipts but the 5th times out, the 4
confirmed transactions are not forwarded downstream until the 5th is resolved.

Additionally, `wait_for_inclusion` polls receipt futures **sequentially** (`first_mut()` in a
while loop). All in-flight receipts are waiting on the network concurrently, but we only
check them in order — a slow tx #1 delays detection of an already-confirmed tx #2.

## Context: How the Rest of the Pipeline Works

Every component in the sequencer implements `PipelineComponent` and is connected to its
neighbours via bounded `mpsc` channels. The channel capacity controls backpressure. This
is the established pattern for independent concurrency across the pipeline.

The L1 sender is already one such component. The proposed change applies the same pattern
one level deeper: the sender's internal phases become two independent tasks connected by
bounded channels.

## Proposed Architecture

Two internal tasks replace the single loop:

```
upstream channel
    ──► Submitter ──► in_flight channel ──► Watcher ──► downstream channel
            ▲                                   │
            └────── resubmit channel ◄──────────┘
```

### Submitter

Responsible for everything that touches the L1 provider:

- Reads `L1SenderCommand<Input>` from the upstream channel.
- Estimates EIP-1559 fees and blob base fee; enforces configured caps
  (`GasBlocked`, `BlobFeeBlocked`).
- Builds, signs, and submits transactions via `send_raw_transaction`.
- Sends each submitted `InFlightTx` (command + tx_hash + receipt_future) to the Watcher
  via the in-flight channel.
- Listens on the resubmit channel for commands the Watcher could not confirm (timeout or
  transient receipt error), and re-queues them ahead of any pending commands.
- Maintains its own `ExponentialBackoff` for transient RPC errors during submission.

The Submitter is the **only** task that holds a reference to the L1 provider.

### Watcher

Responsible for observing receipt futures and routing results:

- Receives `InFlightTx` items from the in-flight channel.
- Tracks all outstanding receipt futures in a `FuturesOrdered`, which polls them
  **concurrently** and yields results in nonce order.
- On a confirmed receipt: sets `MINED_STAGE` on the command and forwards it to the
  downstream channel.
- On a timeout (`WatchTxError::Timeout`) or transient error: sends the command back to
  the Submitter via the resubmit channel for resubmission.
- On a fatal error (tx reverted on L1): returns `Err`, dropping the downstream channel
  sender, which cascades shutdown to adjacent tasks.

The Watcher does **not** hold a reference to the L1 provider. All resubmission goes
through the Submitter.

### Why `FuturesOrdered` and Not `FuturesUnordered`

L1 transactions are nonce-ordered: tx N+1 cannot be mined before tx N. Yielding results
out of nonce order would require the downstream to reorder them, adding complexity. With
`FuturesOrdered`, the Watcher yields results in submission order automatically.

Concurrency is preserved: all futures are polled simultaneously. Only the *delivery* of
results to the downstream is in order.

### Channel Summary

| Channel | Type | Capacity | Direction |
|---|---|---|---|
| upstream | `PeekableReceiver<L1SenderCommand<Input>>` | existing | → Submitter |
| in_flight | `mpsc::Sender<InFlightTx<Input>>` | `command_limit` | Submitter → Watcher |
| resubmit | `mpsc::Sender<Input>` | `command_limit` | Watcher → Submitter |
| downstream | `mpsc::Sender<SignedBatchEnvelope<FriProof>>` | existing | Watcher → |

### Passthrough Commands

Passthrough handling runs before either task starts, exactly as today. Once the first
`SendToL1` command arrives, both tasks are spawned. The passthrough phase is not affected
by this change.

## Error Handling Per Task

### Submitter errors

| Error | Behaviour |
|---|---|
| Transient RPC failure (fee estimation, send) | Log, exponential backoff, retry |
| `GasBlocked` | Enter `GasBlocked` state, sleep 30s, recheck |
| `BlobFeeBlocked` | Enter `BlobFeeBlocked` state, sleep 30s, recheck |
| `NonceTooLow` | Treat as transient (prior resubmit landed); retry |
| Fatal (envelope conversion, revert at send) | Return `Err` |

### Watcher errors

| Error | Behaviour |
|---|---|
| `WatchTxError::Timeout` | Send command to resubmit channel, continue |
| Transient receipt polling error | Send command to resubmit channel, continue |
| Tx reverted on L1 | Return `Err` (fatal) |
| Resubmit channel closed (Submitter died) | Return `Err` |
| Downstream channel closed | Return `Err` |

### Fatal error propagation

When the Watcher returns `Err`:
- The downstream channel sender is dropped.
- The downstream component detects the closed channel and shuts down.

When the Submitter returns `Err`:
- The in-flight channel sender is dropped.
- The Watcher's `in_flight_rx.recv()` returns `None`.
- The Watcher drains any remaining futures, then returns `Err`.

No explicit cancellation token is needed: channel closure cascades shutdown naturally,
consistent with how the rest of the pipeline handles component failure.

## Backoff Independence

Each task has its own `ExponentialBackoff` instance:

- A transient RPC failure in the Submitter does not pause receipt polling in the Watcher.
- A slow or stuck inclusion does not block new tx submissions in the Submitter.

This was not possible with the single-loop design where one backoff controlled the whole
component.

## What This Fixes

| Problem | Before | After |
|---|---|---|
| Phase 2 error blocks phase 3 | Yes | No — tasks are independent |
| Phase 3 error blocks phase 4 | Yes | No — Watcher forwards each confirmed tx immediately |
| Sequential receipt polling | Yes | No — `FuturesOrdered` polls all futures concurrently |
| Slow tx blocks detection of confirmed txs | Yes | No — `FuturesOrdered` yields as each completes |

## What This Enables (Follow-ups)

- **Transaction replacement**: The Submitter already tracks `tx_hash` per in-flight tx.
  Adding explicit nonce management here is the natural next step for replacing stuck txs at
  the same nonce with a higher gas price.
- **Per-task metrics**: Each task can report its own latency and error counters, giving
  finer-grained observability than the current single-component metrics.

## Alternatives Considered

### Fixed loop with accumulated error

Run all four phases every iteration regardless of errors; collect the first non-fatal error
and sleep once at the bottom. Simpler diff, but receipt futures are still polled
sequentially, and a slow tx still delays detection of later confirmed txs.

### Fixed loop + `FuturesOrdered`

Adds concurrent receipt polling to the fixed-loop approach. Solves both the phase-blocking
and sequential-polling problems with a moderate structural change. Does not decompose
responsibilities, making future improvements (tx replacement, per-task backoff) harder to
add cleanly.

## Files to Change

| File | Change |
|---|---|
| `lib/l1_sender/src/lib.rs` | Replace `L1SenderLoop` with `Submitter` and `Watcher` structs; update `run_l1_sender` to spawn both |
| `lib/l1_sender/src/error.rs` | No change to the error types; may add Watcher-specific variants |
| `lib/l1_sender/src/metrics.rs` | Add per-task state labels where useful |
