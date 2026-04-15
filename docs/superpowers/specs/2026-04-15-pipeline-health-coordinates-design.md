# Pipeline Health Coordinates — Design Spec

**Date:** 2026-04-15
**Branch:** aba-adjacent-backpressure
**Context:** Addresses Roman's review feedback on `last_picked` / `last_processed` semantic split,
plus extends the model to correctly represent range-processing components (FRI/SNARK provers)
and batch-level monitoring.

---

## Problem Statement

`ComponentHealth` currently has a single `last_processed_block_number` field that means
different things depending on which component is reporting:

| Component | When `record_processed` fires | Actual semantic |
|---|---|---|
| `TreeManager` | On `recv` from input channel | **Picked** — no work done yet |
| `BlockCanonizer` | On `send` to output channel | **Forwarded** — after canonization |
| `Batcher` | After `output.send(batch_envelope)` | **Batch sealed** |
| `FriJobManager` | After external proof submitted | **Proved** |

As a result, `adjacent_block_lag = upstream.last_processed − downstream.last_processed`
conflates channel occupancy with component processing time. The doc comment says
"recorded at receive time" but that is only true for one of the four cases above.

Additionally, range-processing components (FRI/SNARK provers) work on multiple batches
simultaneously. A single `last_processed` pointer misrepresents them as more lagged than
they really are — the operator cannot distinguish "stuck on batch 50" from "actively
proving batches 51–60".

---

## Goals

1. Clean semantic split: `last_picked` (dequeued from input) vs `last_processed` (fully handled/forwarded).
2. Correct adjacent lag formula: `upstream.last_processed − downstream.last_picked` = pure channel occupancy.
3. In-flight range visibility for range-processing components.
4. Batch-level coordinates for batch-pipeline components.
5. No backward compatibility shims — full clean replacement of old fields.

---

## Data Model

### `BlockTrackingCoordinates`

Used for `last_picked` and `last_processed` on all components (block and batch pipeline alike).
The block-space fields provide a uniform axis for adjacent lag computation across the whole
component array.

```rust
pub struct BlockTrackingCoordinates {
    pub block_number: u64,
    pub timestamp: Option<u64>,
    pub recorded_at: Instant,   // internal only — not serialised in HTTP response
}
```

### `BatchTrackingCoordinates`

Used exclusively for `in_flight_first` / `in_flight_last` on range-processing components
(FriJobManager, SnarkJobManager). Carries the batch number alongside block coordinates
so operators can identify which batches are in-flight without reverse-engineering.

```rust
pub struct BatchTrackingCoordinates {
    pub batch_number: u64,
    pub last_block_number: u64,
    pub timestamp: Option<u64>,
    pub recorded_at: Instant,   // internal only — not serialised in HTTP response
}
```

`batch_number` is `u64` throughout the codebase — no newtype wrapper.

### `ComponentHealth`

Old fields `last_processed_block_number`, `last_processed_block_timestamp`,
`last_processed_block_at` are removed entirely. No compatibility shims.

```rust
pub struct ComponentHealth {
    // State
    pub state: GenericComponentState,
    pub specific_state: &'static str,
    pub state_entered_at: Instant,

    // Block-space progress (all components)
    pub last_picked: Option<BlockTrackingCoordinates>,
    pub last_processed: Option<BlockTrackingCoordinates>,

    // In-flight range (range-processing components only: FriJobManager, SnarkJobManager)
    pub in_flight_first: Option<BatchTrackingCoordinates>,
    pub in_flight_last: Option<BatchTrackingCoordinates>,

    // Batch-space progress (batch-pipeline components only)
    pub batch_number: Option<u64>,
}
```

---

## Reporter API

`ComponentHealthReporter` gains three new methods. `record_processed` keeps the same
signature but now writes to `last_processed: Option<BlockTrackingCoordinates>` instead
of the old flat fields.

```rust
// Existing — updated backing field
fn record_processed(&self, block_number: u64, timestamp: Option<u64>);

// New — same high-watermark guard as record_processed
fn record_picked(&self, block_number: u64, timestamp: Option<u64>);

// New — atomically replaces both in_flight fields.
// Pass (None, None) to clear (e.g. when the prover queue drains).
fn record_in_flight_range(
    &self,
    first: Option<BatchTrackingCoordinates>,
    last: Option<BatchTrackingCoordinates>,
);

// New — no watermark guard; always overwrites (caller ensures monotone progression)
fn record_batch_number(&self, batch_number: u64);
```

---

## TrackedChannel Changes

`recv_and_record` and `recv_many_and_record` on `TrackedUnboundedReceiver` are renamed to
`recv_and_record_picked` / `recv_many_and_record_picked` and call `record_picked` instead
of `record_processed`.

`send_and_record` on `TrackedUnboundedSender` is unchanged — it already fires at
forwarding time, which is correct `record_processed` semantics.

The rename produces compile errors at every old callsite, acting as a forcing function
to ensure no component is missed.

---

## Per-Component Recording Responsibilities

### Block pipeline

| Component | `record_picked` | `record_processed` |
|---|---|---|
| `BlockExecutor` | On `recv` from node command source | After block execution completes |
| `BlockCanonizer` | On `recv` from input | `send_and_record` handles this (fires after forwarding) |
| `BlockApplier` | `recv_and_record_picked` | After storage writes complete |
| `TreeManager` | `recv_and_record_picked` | After tree update completes |

### Batch pipeline

| Component | `record_picked` | `record_processed` | `record_batch_number` | `record_in_flight_range` |
|---|---|---|---|---|
| `Batcher` | On `recv` of first block of new batch | After `output.send(batch_envelope)` | After seal | — |
| `FriJobManager` | On `add_job` | On `submit_proof` (already correct) | On `submit_proof` | After every mutation |
| `SnarkJobManager` | On batch received | On proof submitted (already correct) | On proof submitted | After every mutation |
| `GaplessCommitter` | `recv_and_record_picked` | `send_and_record` (already correct) | On `send_and_record` | — |
| `GaplessL1ProofSender` | `recv_and_record_picked` | `send_and_record` (already correct) | On `send_and_record` | — |

### `ProverJobMap` changes

`ProverJobMap` gains a single query method — it does not own the reporter:

```rust
fn in_flight_range(&self) -> Option<(BatchTrackingCoordinates, BatchTrackingCoordinates)>;
```

`FriJobManager` / `SnarkJobManager` call `self.health_reporter.record_in_flight_range(self.jobs.in_flight_range())` after every mutation point (`add_job`, `submit_proof`, `fake_submit_proof`, assignment timeout). This is consistent with the codebase pattern where the component owns the reporter.

---

## Adjacent Lag Formula

`compute_adjacent_snapshots` in `lib/pipeline_health/src/adjacent.rs` changes from:

```
block_diff = upstream.last_processed_block_number − downstream.last_processed_block_number
```

to:

```
block_diff = upstream.last_processed.block_number − downstream.last_picked.block_number
```

This is Roman's exact proposal: pure channel occupancy, no longer conflated with
component processing time.

Backpressure thresholds (`max_block_lag`, `max_time_lag`) compare against this corrected
adjacent lag — no threshold value changes needed, only the formula.

---

## HTTP Response Shape

`ComponentSnapshot` in `lib/status/src/pipeline.rs` — new fields all use
`#[serde(skip_serializing_if = "Option::is_none")]`. `recorded_at` is never serialised.

Block-pipeline component (no new fields visible):
```json
{
  "name": "block_executor",
  "state": "active",
  "state_duration_secs": 1.2,
  "last_picked":    { "block_number": 100, "timestamp": 1700000100 },
  "last_processed": { "block_number": 100, "timestamp": 1700000100 },
  "head_block_lag": 0,
  "head_time_lag_secs": 0.0
}
```

Batch range-processing component:
```json
{
  "name": "fri_job_manager",
  "state": "active",
  "state_duration_secs": 0.8,
  "last_picked":    { "block_number": 500, "timestamp": 1700000050 },
  "last_processed": { "block_number": 500, "timestamp": 1700000050 },
  "in_flight_first": { "batch_number": 51, "last_block_number": 501, "timestamp": 1700000051 },
  "in_flight_last":  { "batch_number": 60, "last_block_number": 780, "timestamp": 1700000078 },
  "batch_number": 50,
  "head_block_lag": 300,
  "adjacent_block_lag": 0
}
```

The `adjacent_block_lag` of 0 for FriJobManager correctly reflects that the channel ahead
of it is empty — it is actively proving the blocks it has already picked. Previously this
would have shown a large lag, misrepresenting the prover as stuck.

---

## Testing Plan

### `ComponentHealthReporter` unit tests
- `record_picked` high-watermark guard mirrors `record_processed` behaviour
- `record_in_flight_range(None, None)` clears both in-flight fields atomically
- `record_batch_number` with a lower value is a no-op
- `last_picked` and `last_processed` advance independently

### `compute_adjacent_snapshots` unit tests
- Adjacent lag uses `upstream.last_processed − downstream.last_picked`
- Existing tests updated; new test: component with `last_picked` ahead of `last_processed`
  gives correct channel lag (zero, since channel is drained)

### `TrackedUnboundedReceiver` unit tests
- `recv_and_record_picked` populates `last_picked`, not `last_processed`
- Compile-error forcing function validates all callsites are updated

### `pipeline.rs` handler unit tests
- `in_flight_first/last` serialise correctly; absent for block-pipeline components
- `batch_number` absent for block-pipeline components
- Adjacent lag reflects corrected formula

### Integration test (`backpressure.rs`)
- Updated to new field names
- New assertion: FriJobManager `adjacent_block_lag` does not spike while batches are
  in-flight (validates the core correctness fix)
