# Pipeline Health Coordinates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single `last_processed_block_number` field in `ComponentHealth` with a clean `last_picked` / `last_processed` semantic split, add in-flight range tracking for FRI/SNARK range-processing components, and expose batch-level coordinates for batch-pipeline components.

**Architecture:** `BlockTrackingCoordinates` and `BatchTrackingCoordinates` are defined in `zksync_os_observability`. `ComponentHealthReporter` gains `record_picked`, `record_in_flight_range`, and `record_batch_number`. The adjacent lag formula changes from `upstream.last_processed − downstream.last_processed` to `upstream.last_processed − downstream.last_picked`. `ProverJobMap` exposes `in_flight_range()` so `FriJobManager` / `SnarkJobManager` can report in-flight batches after every mutation.

**Tech Stack:** Rust, tokio watch channels, axum, serde.

---

## File Map

| File | Change |
|---|---|
| `lib/observability/src/component_health_reporter.rs` | Add coordinate types, replace old fields, add new reporter methods |
| `lib/observability/src/lib.rs` | Export new types |
| `lib/pipeline/src/tracked_channel.rs` | Rename `recv_and_record` → `recv_and_record_picked` |
| `lib/pipeline_health/src/adjacent.rs` | New formula using separate picked/processed maps |
| `lib/pipeline_health/src/monitor.rs` | Read new fields instead of old flat fields |
| `lib/status/src/pipeline.rs` | New `ComponentSnapshot` shape, updated handler |
| `lib/sequencer/src/execution/block_executor.rs` | Add `record_picked` at recv |
| `lib/sequencer/src/execution/block_canonizer.rs` | Add `record_picked` at recv |
| `lib/sequencer/src/execution/block_applier.rs` | Rename + add `record_processed` after storage writes |
| `node/bin/src/tree_manager.rs` | Rename + add `record_processed` after tree update |
| `node/bin/src/batcher/mod.rs` | Add `record_picked` on first block, `record_batch_number` after seal |
| `node/bin/src/prover_api/prover_job_map/map.rs` | Add `in_flight_range()` |
| `node/bin/src/prover_api/fri_job_manager.rs` | `record_picked`, `record_batch_number`, `record_in_flight_range` |
| `node/bin/src/prover_api/snark_job_manager.rs` | Same |
| `node/bin/src/prover_api/gapless_committer.rs` | `record_picked` at recv, `record_batch_number` in flush loop |
| `node/bin/src/prover_api/gapless_l1_proof_sender.rs` | `record_picked` at recv, `record_batch_number` in flush loop |
| `lib/l1_sender/src/lib.rs` | Add `record_picked` at receive time |
| `integration-tests/tests/backpressure.rs` | Update field references |

---

### Task 1: Core types and `ComponentHealthReporter`

**Files:**
- Modify: `lib/observability/src/component_health_reporter.rs`
- Modify: `lib/observability/src/lib.rs`

- [ ] **Step 1: Write failing tests for the new types and methods**

Add the following tests inside the existing `#[cfg(test)]` block in `lib/observability/src/component_health_reporter.rs`, below the existing tests:

```rust
#[tokio::test]
async fn record_picked_advances_independently_of_processed() {
    let (reporter, rx) = ComponentHealthReporter::new("test");
    reporter.record_picked(5, Some(500));
    reporter.record_processed(3, Some(300));
    let h = rx.borrow();
    assert_eq!(h.last_picked.as_ref().unwrap().block_number, 5);
    assert_eq!(h.last_processed.as_ref().unwrap().block_number, 3);
}

#[tokio::test]
async fn record_picked_high_watermark_guard() {
    let (reporter, rx) = ComponentHealthReporter::new("test");
    reporter.record_picked(10, None);
    reporter.record_picked(5, None); // stale — must be ignored
    assert_eq!(rx.borrow().last_picked.as_ref().unwrap().block_number, 10);
}

#[tokio::test]
async fn record_in_flight_range_stores_both_ends() {
    let (reporter, rx) = ComponentHealthReporter::new("test");
    reporter.record_in_flight_range(
        Some(BatchTrackingCoordinates::new(1, 100, Some(1000))),
        Some(BatchTrackingCoordinates::new(5, 500, Some(5000))),
    );
    let h = rx.borrow();
    assert_eq!(h.in_flight_first.as_ref().unwrap().batch_number, 1);
    assert_eq!(h.in_flight_last.as_ref().unwrap().batch_number, 5);
    assert_eq!(h.in_flight_first.as_ref().unwrap().last_block_number, 100);
    assert_eq!(h.in_flight_last.as_ref().unwrap().last_block_number, 500);
}

#[tokio::test]
async fn record_in_flight_range_clears_with_none() {
    let (reporter, rx) = ComponentHealthReporter::new("test");
    reporter.record_in_flight_range(
        Some(BatchTrackingCoordinates::new(1, 100, None)),
        Some(BatchTrackingCoordinates::new(5, 500, None)),
    );
    reporter.record_in_flight_range(None, None);
    let h = rx.borrow();
    assert!(h.in_flight_first.is_none());
    assert!(h.in_flight_last.is_none());
}

#[tokio::test]
async fn record_batch_number_high_watermark() {
    let (reporter, rx) = ComponentHealthReporter::new("test");
    reporter.record_batch_number(10);
    reporter.record_batch_number(5); // stale — must be ignored
    assert_eq!(rx.borrow().batch_number, Some(10));
}

#[tokio::test]
async fn record_processed_no_longer_uses_flat_fields() {
    // Verifies old flat fields are gone and last_processed is a coordinate type.
    let (reporter, rx) = ComponentHealthReporter::new("test");
    reporter.record_processed(42, Some(999));
    let h = rx.borrow();
    let coord = h.last_processed.as_ref().unwrap();
    assert_eq!(coord.block_number, 42);
    assert_eq!(coord.timestamp, Some(999));
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo nextest run -p zksync_os_observability --no-fail-fast 2>&1 | tail -20
```

Expected: compile errors — `BatchTrackingCoordinates`, `record_picked`, etc. not yet defined.

- [ ] **Step 3: Replace the entire content of `component_health_reporter.rs`**

```rust
use crate::generic_component_state::GenericComponentState;
use crate::state_label::StateLabel;
use tokio::{sync::watch, time::Instant};

/// Block-space coordinates: block number, optional timestamp, and when this
/// coordinate was last recorded (for stall detection in the monitor).
/// `recorded_at` is internal — not serialised in HTTP responses.
#[derive(Clone, Debug)]
pub struct BlockTrackingCoordinates {
    pub block_number: u64,
    pub timestamp: Option<u64>,
    pub(crate) recorded_at: Instant,
}

impl BlockTrackingCoordinates {
    pub fn new(block_number: u64, timestamp: Option<u64>) -> Self {
        Self {
            block_number,
            timestamp,
            recorded_at: Instant::now(),
        }
    }
}

/// Batch-space coordinates for range-processing components (FriJobManager,
/// SnarkJobManager). Carries batch number alongside the batch's last block
/// number and timestamp so operators can identify in-flight batches directly.
/// `recorded_at` is internal — not serialised in HTTP responses.
#[derive(Clone, Debug)]
pub struct BatchTrackingCoordinates {
    pub batch_number: u64,
    pub last_block_number: u64,
    pub timestamp: Option<u64>,
    pub(crate) recorded_at: Instant,
}

impl BatchTrackingCoordinates {
    pub fn new(batch_number: u64, last_block_number: u64, timestamp: Option<u64>) -> Self {
        Self {
            batch_number,
            last_block_number,
            timestamp,
            recorded_at: Instant::now(),
        }
    }
}

/// Health snapshot reported by a pipeline component on every state transition.
#[derive(Clone, Debug)]
pub struct ComponentHealth {
    pub state: GenericComponentState,
    /// Fine-grained state string from the component's StateLabel impl.
    pub specific_state: &'static str,
    /// When the current state was entered (monotonic).
    pub state_entered_at: Instant,

    /// When this component last dequeued an item from its input channel.
    /// Absent until the first item is received. High-watermark semantics.
    pub last_picked: Option<BlockTrackingCoordinates>,

    /// When this component last fully handled/forwarded an item downstream.
    /// Absent until the first item is fully processed. High-watermark semantics.
    pub last_processed: Option<BlockTrackingCoordinates>,

    /// Oldest batch currently in-flight (assigned to an external prover).
    /// Only populated for range-processing components: FriJobManager, SnarkJobManager.
    pub in_flight_first: Option<BatchTrackingCoordinates>,

    /// Newest batch currently in-flight (assigned to an external prover).
    /// Only populated for range-processing components: FriJobManager, SnarkJobManager.
    pub in_flight_last: Option<BatchTrackingCoordinates>,

    /// Last batch number fully processed by this component.
    /// Only populated for batch-pipeline components (Batcher and downstream).
    /// High-watermark semantics.
    pub batch_number: Option<u64>,
}

/// Uses `watch::Sender` — updates are infallible, no background task, no global state.
#[derive(Debug)]
pub struct ComponentHealthReporter {
    sender: watch::Sender<ComponentHealth>,
    component: &'static str,
}

impl ComponentHealthReporter {
    /// Returns the reporter (owned by the component) and the receiver (handed to the monitor).
    pub fn new(component: &'static str) -> (Self, watch::Receiver<ComponentHealth>) {
        let initial = ComponentHealth {
            state: GenericComponentState::Idle,
            specific_state: "idle",
            state_entered_at: Instant::now(),
            last_picked: None,
            last_processed: None,
            in_flight_first: None,
            in_flight_last: None,
            batch_number: None,
        };
        let (sender, receiver) = watch::channel(initial);
        (Self { sender, component }, receiver)
    }

    /// Transition to a new state and record time-in-previous-state metric.
    pub fn enter_state(&self, new_state: impl StateLabel) {
        let now = Instant::now();
        self.sender.send_modify(|health| {
            if health.specific_state == new_state.specific() {
                return;
            }
            let elapsed = now.duration_since(health.state_entered_at);
            crate::metrics::GENERAL_METRICS.component_time_spent_in_state
                [&(self.component, health.state, health.specific_state)]
                .inc_by(elapsed.as_secs_f64());
            health.state = new_state.generic();
            health.specific_state = new_state.specific();
            health.state_entered_at = now;
        });
    }

    /// Record when a block was dequeued from the input channel (before any processing).
    /// High-watermark semantics: stale out-of-order calls are ignored.
    pub fn record_picked(&self, block_number: u64, timestamp: Option<u64>) {
        self.sender.send_if_modified(|health| {
            if let Some(ref current) = health.last_picked {
                if block_number < current.block_number {
                    return false;
                }
            }
            health.last_picked = Some(BlockTrackingCoordinates::new(block_number, timestamp));
            true
        });
    }

    /// Record when a block was fully handled/forwarded downstream.
    /// High-watermark semantics: stale out-of-order calls are ignored.
    pub fn record_processed(&self, block_number: u64, timestamp: Option<u64>) {
        self.sender.send_if_modified(|health| {
            if let Some(ref current) = health.last_processed {
                if block_number < current.block_number {
                    return false;
                }
            }
            health.last_processed = Some(BlockTrackingCoordinates::new(block_number, timestamp));
            true
        });
    }

    /// Record the current in-flight range for range-processing components.
    /// Atomically replaces both `in_flight_first` and `in_flight_last`.
    /// Pass `(None, None)` to clear (e.g. when the prover queue drains).
    pub fn record_in_flight_range(
        &self,
        first: Option<BatchTrackingCoordinates>,
        last: Option<BatchTrackingCoordinates>,
    ) {
        self.sender.send_modify(|health| {
            health.in_flight_first = first;
            health.in_flight_last = last;
        });
    }

    /// Record the last completed batch number for batch-pipeline components.
    /// High-watermark semantics: stale out-of-order calls are ignored.
    pub fn record_batch_number(&self, batch_number: u64) {
        self.sender.send_if_modified(|health| {
            if let Some(current) = health.batch_number {
                if batch_number < current {
                    return false;
                }
            }
            health.batch_number = Some(batch_number);
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GenericComponentState;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn reporter_new_starts_in_idle() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        let health = rx.borrow().clone();
        assert_eq!(health.state, GenericComponentState::Idle);
        assert_eq!(health.specific_state, "idle");
        assert!(health.last_picked.is_none());
        assert!(health.last_processed.is_none());
        drop(reporter);
    }

    #[tokio::test]
    async fn enter_state_updates_receiver() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        reporter.enter_state(GenericComponentState::Active);
        assert_eq!(rx.borrow().state, GenericComponentState::Active);
    }

    #[tokio::test]
    async fn record_processed_updates_coord() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        reporter.record_processed(42, Some(1_700_000_000));
        let h = rx.borrow();
        let coord = h.last_processed.as_ref().unwrap();
        assert_eq!(coord.block_number, 42);
        assert_eq!(coord.timestamp, Some(1_700_000_000));
    }

    #[tokio::test]
    async fn record_processed_high_watermark() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_processed(100, Some(1_000));
        reporter.record_processed(80, Some(800)); // stale
        assert_eq!(rx.borrow().last_processed.as_ref().unwrap().block_number, 100);
        assert_eq!(rx.borrow().last_processed.as_ref().unwrap().timestamp, Some(1_000));
    }

    #[tokio::test]
    async fn record_processed_accepts_equal_block_number() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_processed(50, Some(500));
        reporter.record_processed(50, Some(501));
        assert_eq!(rx.borrow().last_processed.as_ref().unwrap().timestamp, Some(501));
    }

    #[tokio::test]
    async fn state_entered_at_updates_on_enter_state() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        let t0 = rx.borrow().state_entered_at;
        sleep(Duration::from_millis(10)).await;
        reporter.enter_state(GenericComponentState::Active);
        assert!(rx.borrow().state_entered_at > t0);
    }

    #[tokio::test]
    async fn enter_state_same_state_does_not_reset_timer() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        let t0 = rx.borrow().state_entered_at;
        tokio::time::sleep(Duration::from_millis(10)).await;
        reporter.enter_state(GenericComponentState::Idle);
        assert_eq!(rx.borrow().state_entered_at, t0);
    }

    #[tokio::test]
    async fn multiple_reporters_independent() {
        let (r1, rx1) = ComponentHealthReporter::new("c1");
        let (r2, rx2) = ComponentHealthReporter::new("c2");
        r1.record_processed(10, None);
        r2.record_processed(20, None);
        assert_eq!(rx1.borrow().last_processed.as_ref().unwrap().block_number, 10);
        assert_eq!(rx2.borrow().last_processed.as_ref().unwrap().block_number, 20);
    }

    // --- New tests ---

    #[tokio::test]
    async fn record_picked_advances_independently_of_processed() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_picked(5, Some(500));
        reporter.record_processed(3, Some(300));
        let h = rx.borrow();
        assert_eq!(h.last_picked.as_ref().unwrap().block_number, 5);
        assert_eq!(h.last_processed.as_ref().unwrap().block_number, 3);
    }

    #[tokio::test]
    async fn record_picked_high_watermark_guard() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_picked(10, None);
        reporter.record_picked(5, None);
        assert_eq!(rx.borrow().last_picked.as_ref().unwrap().block_number, 10);
    }

    #[tokio::test]
    async fn record_in_flight_range_stores_both_ends() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_in_flight_range(
            Some(BatchTrackingCoordinates::new(1, 100, Some(1000))),
            Some(BatchTrackingCoordinates::new(5, 500, Some(5000))),
        );
        let h = rx.borrow();
        assert_eq!(h.in_flight_first.as_ref().unwrap().batch_number, 1);
        assert_eq!(h.in_flight_last.as_ref().unwrap().batch_number, 5);
        assert_eq!(h.in_flight_first.as_ref().unwrap().last_block_number, 100);
        assert_eq!(h.in_flight_last.as_ref().unwrap().last_block_number, 500);
    }

    #[tokio::test]
    async fn record_in_flight_range_clears_with_none() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_in_flight_range(
            Some(BatchTrackingCoordinates::new(1, 100, None)),
            Some(BatchTrackingCoordinates::new(5, 500, None)),
        );
        reporter.record_in_flight_range(None, None);
        let h = rx.borrow();
        assert!(h.in_flight_first.is_none());
        assert!(h.in_flight_last.is_none());
    }

    #[tokio::test]
    async fn record_batch_number_high_watermark() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_batch_number(10);
        reporter.record_batch_number(5);
        assert_eq!(rx.borrow().batch_number, Some(10));
    }

    #[tokio::test]
    async fn record_processed_no_longer_uses_flat_fields() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_processed(42, Some(999));
        let h = rx.borrow();
        let coord = h.last_processed.as_ref().unwrap();
        assert_eq!(coord.block_number, 42);
        assert_eq!(coord.timestamp, Some(999));
    }
}
```

- [ ] **Step 4: Update exports in `lib/observability/src/lib.rs`**

Replace the existing `component_health_reporter` pub-use line:

```rust
// old:
pub use component_health_reporter::{ComponentHealth, ComponentHealthReporter};

// new:
pub use component_health_reporter::{
    BatchTrackingCoordinates, BlockTrackingCoordinates, ComponentHealth, ComponentHealthReporter,
};
```

- [ ] **Step 5: Run the observability tests**

```bash
cargo nextest run -p zksync_os_observability 2>&1 | tail -20
```

Expected: all tests pass. Other crates will have compile errors — that's expected and fixed in subsequent tasks.

- [ ] **Step 6: Commit**

```bash
git add lib/observability/src/component_health_reporter.rs lib/observability/src/lib.rs
git commit -m "feat(observability): introduce BlockTrackingCoordinates and BatchTrackingCoordinates

Replace flat last_processed_block_number/timestamp/at fields with typed
last_picked and last_processed BlockTrackingCoordinates. Add in_flight_first/
last BatchTrackingCoordinates for range-processing components, and batch_number
for batch-pipeline components.

Add record_picked, record_in_flight_range, record_batch_number methods.
All use high-watermark semantics. Old flat fields are gone."
```

---

### Task 2: Rename `recv_and_record` → `recv_and_record_picked` in `TrackedChannel`

**Files:**
- Modify: `lib/pipeline/src/tracked_channel.rs`

- [ ] **Step 1: Rename both methods and update their internals**

In `TrackedUnboundedReceiver`:

Replace the `recv_and_record` method:
```rust
// old name + old call:
pub async fn recv_and_record(
    &mut self,
    reporter: &zksync_os_observability::ComponentHealthReporter,
) -> Option<T> {
    let item = self.recv().await?;
    reporter.record_processed(item.block_number(), item.block_timestamp());
    Some(item)
}
```
with:
```rust
// new name + record_picked:
pub async fn recv_and_record_picked(
    &mut self,
    reporter: &zksync_os_observability::ComponentHealthReporter,
) -> Option<T> {
    let item = self.recv().await?;
    reporter.record_picked(item.block_number(), item.block_timestamp());
    Some(item)
}
```

Replace the `recv_many_and_record` method:
```rust
// old name:
pub async fn recv_many_and_record(
    &mut self,
    buf: &mut Vec<T>,
    limit: usize,
    reporter: &zksync_os_observability::ComponentHealthReporter,
) -> usize {
    let start = buf.len();
    let n = self.recv_many(buf, limit).await;
    if n > 0 {
        let last = &buf[start + n - 1];
        reporter.record_processed(last.block_number(), last.block_timestamp());
    }
    n
}
```
with:
```rust
// new name + record_picked:
pub async fn recv_many_and_record_picked(
    &mut self,
    buf: &mut Vec<T>,
    limit: usize,
    reporter: &zksync_os_observability::ComponentHealthReporter,
) -> usize {
    let start = buf.len();
    let n = self.recv_many(buf, limit).await;
    if n > 0 {
        let last = &buf[start + n - 1];
        reporter.record_picked(last.block_number(), last.block_timestamp());
    }
    n
}
```

- [ ] **Step 2: Update the test names and assertions in the same file**

In the `#[cfg(test)]` block, find `recv_and_record_calls_reporter` and update:

```rust
#[tokio::test]
async fn recv_and_record_picked_calls_reporter() {
    // ... (same body) ...
    let item = rx.recv_and_record_picked(&reporter).await.unwrap();
    assert_eq!(item.seq, 10);
    // record_picked populates last_picked, not last_processed
    assert_eq!(health_rx.borrow().last_picked.as_ref().unwrap().block_number, Some(10));  // wrong — fix:
    assert_eq!(
        health_rx.borrow().last_picked.as_ref().map(|c| c.block_number),
        Some(10)
    );
    assert_eq!(
        health_rx.borrow().last_picked.as_ref().and_then(|c| c.timestamp),
        Some(1000)
    );
}
```

Also rename and update the `recv_many_and_record_tests` module: rename to `recv_many_and_record_picked_tests`, rename the inner test functions to use `recv_and_record_picked` / `recv_many_and_record_picked`, and update assertions from `last_processed_block_number` to `last_picked.as_ref().map(|c| c.block_number)`.

- [ ] **Step 3: Run pipeline crate tests**

```bash
cargo nextest run -p zksync_os_pipeline 2>&1 | tail -20
```

Expected: pipeline crate tests pass. Downstream crates that called `recv_and_record` will now produce compile errors — those are fixed in Tasks 4–8.

- [ ] **Step 4: Commit**

```bash
git add lib/pipeline/src/tracked_channel.rs
git commit -m "refactor(pipeline): rename recv_and_record → recv_and_record_picked

The old name implied 'processed' semantics but actually fired at dequeue
time. The new name is explicit: recording at pick time, before any work.
Downstream compile errors will be resolved in subsequent commits."
```

---

### Task 3: Update adjacent lag formula and pipeline health monitor

**Files:**
- Modify: `lib/pipeline_health/src/adjacent.rs`
- Modify: `lib/pipeline_health/src/monitor.rs`

- [ ] **Step 1: Update `compute_adjacent_snapshots` signature and formula**

In `lib/pipeline_health/src/adjacent.rs`, replace the entire function signature and body:

```rust
// old signature:
pub fn compute_adjacent_snapshots(
    adjacency: &[(ComponentId, ComponentId)],
    snapshots: &HashMap<ComponentId, (u64, Option<u64>)>,
) -> HashMap<ComponentId, AdjacentSnapshot> {
    // ...
    let block_diff = up_seq.saturating_sub(down_seq);
    // ...
}
```

New version — takes separate maps for processed and picked:

```rust
/// Compute adjacent block and time diffs for each downstream component.
///
/// `adjacency` is a slice of (upstream, downstream) pairs.
/// `processed` maps each ComponentId to its last_processed (block_number, timestamp).
/// `picked` maps each ComponentId to its last_picked (block_number, timestamp).
///
/// Block diff = upstream.last_processed − downstream.last_picked
/// This gives pure channel occupancy: blocks forwarded by upstream but not yet
/// dequeued by downstream.
pub fn compute_adjacent_snapshots(
    adjacency: &[(ComponentId, ComponentId)],
    processed: &HashMap<ComponentId, (u64, Option<u64>)>,
    picked: &HashMap<ComponentId, (u64, Option<u64>)>,
) -> HashMap<ComponentId, AdjacentSnapshot> {
    {
        let mut seen = std::collections::HashSet::new();
        for &(_, down) in adjacency {
            assert!(
                seen.insert(down),
                "fan-in topology detected: downstream component {:?} appears in multiple \
                 adjacency pairs; compute_adjacent_snapshots does not support this",
                down
            );
        }
    }

    adjacency
        .iter()
        .filter_map(|&(up, down)| {
            let &(up_seq, up_ts) = processed.get(&up)?;
            let &(down_seq, down_ts) = picked.get(&down)?;
            let block_diff = up_seq.saturating_sub(down_seq);
            let time_diff = match (up_ts, down_ts) {
                (Some(u), Some(d)) => Some(Duration::from_secs(u.saturating_sub(d))),
                _ => None,
            };
            Some((down, AdjacentSnapshot { block_diff, time_diff }))
        })
        .collect()
}
```

- [ ] **Step 2: Update all unit tests in `adjacent.rs`**

Each test that calls `compute_adjacent_snapshots` now passes two maps. The `snapshots` map becomes the `processed` map; add a matching `picked` map (same values for simplicity — tests verify the formula, not the map separation):

```rust
// Example: replace every test call like:
let result = compute_adjacent_snapshots(&adjacency, &snapshots);
// with:
let result = compute_adjacent_snapshots(&adjacency, &snapshots, &snapshots);
```

Add one new test specifically for the formula using different picked vs processed values:

```rust
#[test]
fn block_diff_uses_upstream_processed_minus_downstream_picked() {
    let mut processed = HashMap::new();
    processed.insert(ComponentId::BlockExecutor, snap(100, None));

    let mut picked = HashMap::new();
    // downstream picked at 95 (5 blocks sitting in the channel)
    picked.insert(ComponentId::BlockCanonizer, snap(95, None));

    // processed for downstream doesn't matter for this diff
    let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
    let result = compute_adjacent_snapshots(&adjacency, &processed, &picked);
    assert_eq!(result[&ComponentId::BlockCanonizer].block_diff, 5);
}
```

- [ ] **Step 3: Update `monitor.rs` to read new fields**

In `lib/pipeline_health/src/monitor.rs`, find all reads of `last_processed_block_number` and `last_processed_block_timestamp` and replace with reads from `last_processed`:

```rust
// old pattern:
h.last_processed_block_number.unwrap_or(0)
h.last_processed_block_timestamp

// new pattern:
h.last_processed.as_ref().map(|c| c.block_number).unwrap_or(0)
h.last_processed.as_ref().and_then(|c| c.timestamp)
```

Also find any reads of `last_processed_block_at` (used for stall detection) and replace with:
```rust
// old:
h.last_processed_block_at

// new:
h.last_processed.as_ref().map(|c| c.recorded_at)
```

Update all test calls to `reporter.record_processed(n, ts)` — the method signature is unchanged so these compile, but verify tests still pass.

- [ ] **Step 4: Run pipeline_health tests**

```bash
cargo nextest run -p zksync_os_pipeline_health 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add lib/pipeline_health/src/adjacent.rs lib/pipeline_health/src/monitor.rs
git commit -m "feat(pipeline_health): adjacent lag now uses upstream.last_processed − downstream.last_picked

Pure channel occupancy formula. compute_adjacent_snapshots takes separate
processed and picked maps. Fixes the conflation of channel lag with component
processing time identified in Roman's review."
```

---

### Task 4: Update HTTP response shape

**Files:**
- Modify: `lib/status/src/pipeline.rs`

- [ ] **Step 1: Update `ComponentSnapshot` struct**

Replace the current struct definition:

```rust
#[derive(Serialize)]
pub struct ComponentSnapshot {
    pub state: &'static str,
    pub state_duration_secs: f64,
    /// Block number last dequeued from the input channel (before any processing).
    /// High-watermark; absent until first item received.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_picked_block: Option<u64>,
    /// Timestamp of the last picked block. Absent if component hasn't reported timestamps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_picked_timestamp: Option<u64>,
    /// Block number last fully handled/forwarded downstream.
    /// High-watermark; absent until first item processed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_processed_block: Option<u64>,
    /// Timestamp of the last processed block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_processed_timestamp: Option<u64>,
    /// Oldest batch currently in-flight (range-processing components only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_first: Option<InFlightBatchJson>,
    /// Newest batch currently in-flight (range-processing components only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_last: Option<InFlightBatchJson>,
    /// Last completed batch number (batch-pipeline components only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_number: Option<u64>,
    /// Blocks behind the pipeline head (BlockExecutor). Always present.
    pub head_block_lag: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjacent_block_lag: Option<u64>,
    pub head_time_lag_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjacent_time_lag_secs: Option<f64>,
}

#[derive(Serialize)]
pub struct InFlightBatchJson {
    pub batch_number: u64,
    pub last_block_number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}
```

- [ ] **Step 2: Update the `pipeline` handler**

In the `pipeline` handler function, replace the `component_snapshots` map building and the `compute_adjacent_snapshots` call:

```rust
// Build two separate maps for the new formula
let processed_snapshots: std::collections::HashMap<ComponentId, (u64, Option<u64>)> = state
    .component_health
    .iter()
    .map(|(id, rx)| {
        let h = rx.borrow();
        let (num, ts) = h
            .last_processed
            .as_ref()
            .map(|c| (c.block_number, c.timestamp))
            .unwrap_or((0, None));
        (*id, (num, ts))
    })
    .collect();

let picked_snapshots: std::collections::HashMap<ComponentId, (u64, Option<u64>)> = state
    .component_health
    .iter()
    .map(|(id, rx)| {
        let h = rx.borrow();
        let (num, ts) = h
            .last_picked
            .as_ref()
            .map(|c| (c.block_number, c.timestamp))
            .unwrap_or((0, None));
        (*id, (num, ts))
    })
    .collect();

let adjacent = compute_adjacent_snapshots(&state.adjacency, &processed_snapshots, &picked_snapshots);
```

Also update head block extraction:
```rust
let (head_block, head_ts) = state
    .component_health
    .iter()
    .find(|(id, _)| *id == ComponentId::BlockExecutor)
    .map(|(_, rx)| {
        let h = rx.borrow();
        (
            h.last_processed.as_ref().map(|c| c.block_number).unwrap_or(0),
            h.last_processed.as_ref().and_then(|c| c.timestamp),
        )
    })
    .unwrap_or((0, None));
```

Update the `ComponentSnapshot` construction inside the `.map()`:

```rust
let last_processed_num = h.last_processed.as_ref().map(|c| c.block_number).unwrap_or(0);
let last_picked_num = h.last_picked.as_ref().map(|c| c.block_number).unwrap_or(0);
let head_block_lag = head_block.saturating_sub(last_processed_num);
let head_time_lag_secs = match (
    h.last_processed.as_ref().and_then(|c| c.timestamp),
    head_ts,
) {
    (Some(comp_ts), Some(h_ts)) => h_ts.saturating_sub(comp_ts) as f64,
    _ => 0.0,
};

ComponentEntryWithThresholds {
    name: id.as_str(),
    snapshot: ComponentSnapshot {
        state: h.state.as_str(),
        state_duration_secs: elapsed,
        last_picked_block: h.last_picked.as_ref().map(|c| c.block_number),
        last_picked_timestamp: h.last_picked.as_ref().and_then(|c| c.timestamp),
        last_processed_block: h.last_processed.as_ref().map(|c| c.block_number),
        last_processed_timestamp: h.last_processed.as_ref().and_then(|c| c.timestamp),
        in_flight_first: h.in_flight_first.as_ref().map(|c| InFlightBatchJson {
            batch_number: c.batch_number,
            last_block_number: c.last_block_number,
            timestamp: c.timestamp,
        }),
        in_flight_last: h.in_flight_last.as_ref().map(|c| InFlightBatchJson {
            batch_number: c.batch_number,
            last_block_number: c.last_block_number,
            timestamp: c.timestamp,
        }),
        batch_number: h.batch_number,
        head_block_lag,
        adjacent_block_lag: adjacent.get(id).map(|s| s.block_diff),
        head_time_lag_secs,
        adjacent_time_lag_secs: adjacent
            .get(id)
            .and_then(|s| s.time_diff)
            .map(|d| d.as_secs_f64()),
    },
    thresholds: ThresholdsJson {
        max_block_lag: cond.max_block_lag,
        max_time_lag_secs: cond.max_time_lag.map(|d| d.as_secs_f64()),
    },
}
```

- [ ] **Step 3: Update unit tests in `pipeline.rs`**

In the test helpers, replace `reporter.record_processed(n, ts)` calls — the method signature is unchanged so they compile. Update assertions that referenced the old `last_processed_block` field name to `last_processed_block` (which is now `Option<u64>` backed by `last_processed.block_number`).

Update the `upstream_diff_reflects_adjacent_lag_not_head_lag` test: after calling `record_processed` on reporters, also call `record_picked` with the same value so `picked_snapshots` is populated:

```rust
exec_reporter.record_processed(100, None);
exec_reporter.record_picked(100, None);

applier_reporter.record_processed(90, None);
applier_reporter.record_picked(90, None);

canonizer_reporter.record_processed(95, None);
canonizer_reporter.record_picked(95, None);
```

Add a new test to verify `adjacent_block_lag` of 0 for a component that has `last_picked` equal to upstream's `last_processed`:

```rust
#[tokio::test]
async fn adjacent_lag_zero_when_channel_drained() {
    // upstream processed=100, downstream picked=100 → channel is empty → adjacent_lag=0
    // even though downstream.last_processed may be lower
    let (_stop_tx, stop_rx) = watch::channel(false);
    let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);

    let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
    exec_reporter.record_processed(100, None);
    exec_reporter.record_picked(100, None);

    let (applier_reporter, applier_rx) = ComponentHealthReporter::new("block_applier");
    applier_reporter.record_picked(100, None);   // picked everything the executor produced
    applier_reporter.record_processed(80, None); // but only finished 80 so far

    let state = AppState {
        stop_receiver: stop_rx,
        acceptance_state: accept_rx,
        component_health: Arc::new(vec![
            (ComponentId::BlockExecutor, exec_rx),
            (ComponentId::BlockApplier, applier_rx),
        ]),
        pipeline_health_config: PipelineHealthConfig::default(),
        adjacency: Arc::new(vec![
            (ComponentId::BlockExecutor, ComponentId::BlockApplier),
        ]),
    };

    let Json(body) = pipeline(State(state)).await;
    let applier = body.components.iter().find(|c| c.name == "block_applier").unwrap();
    // channel is empty (upstream processed == downstream picked) → lag = 0
    assert_eq!(applier.snapshot.adjacent_block_lag, Some(0));
    // head lag still reflects that applier is 20 blocks behind on processing
    assert_eq!(applier.snapshot.head_block_lag, 20);
}
```

- [ ] **Step 4: Run status crate tests**

```bash
cargo nextest run -p zksync_os_server 2>&1 | grep -E "PASS|FAIL|error" | tail -30
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add lib/status/src/pipeline.rs
git commit -m "feat(status): pipeline endpoint exposes last_picked, last_processed, in_flight range

ComponentSnapshot now exposes semantically correct last_picked_block /
last_processed_block split. in_flight_first/last carry batch coordinates
for FRI/SNARK components. adjacent_block_lag computed from
upstream.last_processed − downstream.last_picked (pure channel occupancy)."
```

---

### Task 5: Block pipeline components

**Files:**
- Modify: `lib/sequencer/src/execution/block_executor.rs`
- Modify: `lib/sequencer/src/execution/block_canonizer.rs`
- Modify: `lib/sequencer/src/execution/block_applier.rs`
- Modify: `node/bin/src/tree_manager.rs`

- [ ] **Step 1: `block_executor.rs` — add `record_picked` at recv**

In `BlockExecutor::run`, the existing `input.recv()` at line 76 becomes:

```rust
let Some(cmd) = input.recv().await else {
    tracing::info!("inbound channel closed");
    return Ok(());
};
// Record pick immediately after dequeue, before any processing
self.health_reporter
    .record_picked(cmd.block_number(), cmd.block_timestamp());
```

Where `cmd.block_number()` and `cmd.block_timestamp()` come from `HasBlockRangeEnd` on `BlockCommand`. Confirm `BlockCommand` implements this trait; if not, extract block number from the command directly. The existing `send_and_record` call at the bottom already handles `record_processed`.

- [ ] **Step 2: `block_canonizer.rs` — add `record_picked` at both recv arms**

In `BlockCanonizer::run`, the `input.recv()` select arm uses a raw receive. Add `record_picked` immediately after the item is matched:

```rust
maybe_executed = input.recv(), if produced_queue.len() < MAX_PRODUCED_QUEUE_SIZE => {
    let Some(BlockPayload {
        output: block_output,
        record: replay_record,
        command_type: cmd_type,
    }) = maybe_executed
    else {
        tracing::info!("inbound channel closed");
        return Ok(());
    };
    // Record pick: block dequeued from upstream, not yet canonized or sent downstream
    self.health_reporter.record_picked(
        replay_record.block_context.block_number,
        Some(replay_record.block_context.timestamp),
    );
    // existing match on cmd_type follows...
```

The `send_and_record` calls later in both Replay and Produce arms already handle `record_processed`.

- [ ] **Step 3: `block_applier.rs` — rename + add `record_processed`**

Change the `recv_and_record` call to `recv_and_record_picked`:

```rust
// old:
let Some(BlockPayload { ... }) = input.recv_and_record(&self.health_reporter).await
// new:
let Some(BlockPayload { ... }) = input.recv_and_record_picked(&self.health_reporter).await
```

Then at the end of the loop body, after `self.applied_block_number_sender.send_replace(block_number)` and after the `output.send(...)` call, add:

```rust
// Record processed after all storage writes are complete and block sent downstream
self.health_reporter.record_processed(
    block_number,
    Some(executed_replay.block_context.timestamp),
);
```

Also remove the now-stale doc comment above the old `recv_and_record` call that said "marks this block as processed at receive time — before storage writes" — that was the incorrect behaviour we're fixing.

- [ ] **Step 4: `tree_manager.rs` — rename + add `record_processed`**

```rust
// old:
}) = input.recv_and_record(&self.health_reporter).await
// new:
}) = input.recv_and_record_picked(&self.health_reporter).await
```

Then after the tree update logic completes (at the end of the loop body, before looping back), add:

```rust
self.health_reporter.record_processed(
    replay_record.block_context.block_number,
    Some(replay_record.block_context.timestamp),
);
```

- [ ] **Step 5: Build check**

```bash
cargo build -p zksync_os_sequencer 2>&1 | grep "^error" | head -20
```

Expected: clean build.

- [ ] **Step 6: Commit**

```bash
git add lib/sequencer/src/execution/block_executor.rs \
        lib/sequencer/src/execution/block_canonizer.rs \
        lib/sequencer/src/execution/block_applier.rs \
        node/bin/src/tree_manager.rs
git commit -m "feat(sequencer): wire record_picked + record_processed for block pipeline components

BlockExecutor, BlockCanonizer: record_picked at recv.
BlockApplier, TreeManager: record_picked at recv (renamed from recv_and_record),
record_processed after storage writes/tree update complete."
```

---

### Task 6: `ProverJobMap.in_flight_range()`

**Files:**
- Modify: `node/bin/src/prover_api/prover_job_map/map.rs`

- [ ] **Step 1: Add the method**

In `impl<T: Clone> ProverJobMap<T>`, add after `get_prover_input`:

```rust
/// Returns the current in-flight range as (first, last) BatchTrackingCoordinates,
/// or None if the queue is empty.
/// First = oldest batch (lowest batch_number), Last = newest batch (highest batch_number).
pub async fn in_flight_range(
    &self,
) -> Option<(
    zksync_os_observability::BatchTrackingCoordinates,
    zksync_os_observability::BatchTrackingCoordinates,
)> {
    let jobs = self.jobs.lock().await;
    if jobs.is_empty() {
        return None;
    }
    // BTreeMap is ordered by key (batch_number), so first/last give min/max.
    let (_, first_entry) = jobs.iter().next().unwrap();
    let (_, last_entry) = jobs.iter().next_back().unwrap();

    let make_coord = |entry: &JobEntry<T>| {
        zksync_os_observability::BatchTrackingCoordinates::new(
            entry.batch_envelope.batch_number(),
            entry.batch_envelope.batch.last_block_number,
            Some(entry.batch_envelope.batch.batch_info.last_block_timestamp),
        )
    };

    Some((make_coord(first_entry), make_coord(last_entry)))
}
```

Add `use zksync_os_observability;` to the top of `map.rs` if not already present.

- [ ] **Step 2: Build check**

```bash
cargo build --bin node 2>&1 | grep "^error" | head -20
```

Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add node/bin/src/prover_api/prover_job_map/map.rs
git commit -m "feat(prover_job_map): add in_flight_range() query method

Returns BatchTrackingCoordinates for the oldest and newest in-flight batch,
or None if the queue is empty. Used by FriJobManager and SnarkJobManager
to update ComponentHealth after every mutation."
```

---

### Task 7: `FriJobManager` — full health wiring

**Files:**
- Modify: `node/bin/src/prover_api/fri_job_manager.rs`

- [ ] **Step 1: Add `record_in_flight_range` helper**

Add a private method to `FriJobManager` that calls both pieces atomically:

```rust
async fn update_in_flight_health(&self) {
    let range = self.jobs.in_flight_range().await;
    let (first, last) = match range {
        Some((f, l)) => (Some(f), Some(l)),
        None => (None, None),
    };
    self.health_reporter.record_in_flight_range(first, last);
}
```

- [ ] **Step 2: `add_job` — record_picked + update in-flight**

After the existing `self.jobs.add_job(batch_envelope).await` call, add:

```rust
pub async fn add_job(&self, batch_envelope: SignedBatchEnvelope<ProverInput>) {
    // Capture coordinates before moving into add_job
    let batch_number = batch_envelope.batch_number();
    let last_block = batch_envelope.batch.last_block_number;
    let timestamp = Some(batch_envelope.batch.batch_info.last_block_timestamp);

    self.jobs.add_job(batch_envelope).await;

    self.health_reporter.record_picked(last_block, timestamp);
    self.update_in_flight_health().await;
}
```

- [ ] **Step 3: `submit_proof` — record_batch_number + update in-flight**

After the existing `self.health_reporter.record_processed(last_block, Some(last_block_timestamp))` call:

```rust
permit.send(envelope);
self.health_reporter
    .record_processed(last_block, Some(last_block_timestamp));
self.health_reporter
    .record_batch_number(removed_job.batch_number());
self.update_in_flight_health().await;
```

- [ ] **Step 4: Update fake submit and timeout reassignment**

Find `submit_fake_proof` (or equivalent fake proof submission method) and add the same three calls after the proof is forwarded downstream. Find any assignment-timeout handling in `pick_next_job` — no health update needed there since the batch is just reassigned, not completed.

- [ ] **Step 5: Build check**

```bash
cargo build --bin node 2>&1 | grep "^error" | head -20
```

- [ ] **Step 6: Commit**

```bash
git add node/bin/src/prover_api/fri_job_manager.rs
git commit -m "feat(fri_job_manager): wire record_picked, record_batch_number, record_in_flight_range

record_picked fires when add_job receives a batch.
record_processed + record_batch_number fire when a proof is submitted.
record_in_flight_range updated after every mutation via in_flight_range()."
```

---

### Task 8: `SnarkJobManager` — full health wiring

**Files:**
- Modify: `node/bin/src/prover_api/snark_job_manager.rs`

- [ ] **Step 1: Add `update_in_flight_health` helper**

Same pattern as FriJobManager:

```rust
async fn update_in_flight_health(&self) {
    let range = self.jobs.in_flight_range().await;
    let (first, last) = match range {
        Some((f, l)) => (Some(f), Some(l)),
        None => (None, None),
    };
    self.health_reporter.record_in_flight_range(first, last);
}
```

- [ ] **Step 2: `add_job` — record_picked + update in-flight**

```rust
pub async fn add_job(&self, batch_envelope: SignedBatchEnvelope<FriProof>) {
    let last_block = batch_envelope.batch.last_block_number;
    let timestamp = Some(batch_envelope.batch.batch_info.last_block_timestamp);

    self.jobs.add_job(batch_envelope).await;

    self.health_reporter.record_picked(last_block, timestamp);
    self.update_in_flight_health().await;
}
```

- [ ] **Step 3: `submit_proof` — add `record_batch_number` + update in-flight**

Locate the `send_downstream` call and the existing `record_processed` call. After both, add:

```rust
self.health_reporter
    .record_batch_number(batch_to); // last batch number in the submitted range
self.update_in_flight_health().await;
```

- [ ] **Step 4: Build check**

```bash
cargo build --bin node 2>&1 | grep "^error" | head -20
```

- [ ] **Step 5: Commit**

```bash
git add node/bin/src/prover_api/snark_job_manager.rs
git commit -m "feat(snark_job_manager): wire record_picked, record_batch_number, record_in_flight_range"
```

---

### Task 9: `Batcher` — block pick + batch completion

**Files:**
- Modify: `node/bin/src/batcher/mod.rs`

- [ ] **Step 1: Locate block recv inside `create_batch`**

`create_batch` calls `block_receiver.recv_many(...)` or similar to accumulate blocks. Find the first block receive inside the batch-building loop and add a `record_picked` call for the first block of each batch cycle.

In `Batcher::run`, find where `create_batch` is called and where the batch starts being assembled. The pattern is roughly:

```rust
// In the batch accumulation loop (inside create_batch or Batcher::run):
// When the FIRST block of a new batch is dequeued, record pick:
if batch_accumulator.is_empty() {
    self.health_reporter.record_picked(block.block_number(), block.block_timestamp());
}
```

Adjust to match the actual shape of `create_batch` — the exact insertion point is wherever the first block of a new batch enters the accumulator.

- [ ] **Step 2: After seal, add `record_batch_number`**

After the existing `self.health_reporter.record_processed(last_block_number, Some(last_block_timestamp))` call (after `output.send(batch_envelope)`):

```rust
if output.send(batch_envelope).is_err() { ... }
self.health_reporter
    .record_processed(last_block_number, Some(last_block_timestamp));
self.health_reporter
    .record_batch_number(batch_number); // batch_number from batch_envelope before move
```

Capture `batch_number` before moving `batch_envelope` into `output.send`.

- [ ] **Step 3: Build check**

```bash
cargo build --bin node 2>&1 | grep "^error" | head -20
```

- [ ] **Step 4: Commit**

```bash
git add node/bin/src/batcher/mod.rs
git commit -m "feat(batcher): add record_picked on first block of batch, record_batch_number on seal"
```

---

### Task 10: `GaplessCommitter`, `GaplessL1ProofSender`, and `L1Sender`

**Files:**
- Modify: `node/bin/src/prover_api/gapless_committer.rs`
- Modify: `node/bin/src/prover_api/gapless_l1_proof_sender.rs`
- Modify: `lib/l1_sender/src/lib.rs`

- [ ] **Step 1: `gapless_committer.rs` — record_picked + record_batch_number**

After the existing `let Some(batch) = input.recv().await else { ... }` line:

```rust
let Some(batch) = input.recv().await else {
    tracing::info!("inbound channel closed");
    return Ok(());
};
// Record pick: batch arrived from upstream (may go into reorder buffer before forwarding)
self.health_reporter.record_picked(
    batch.batch.last_block_number,
    Some(batch.batch.batch_info.last_block_timestamp),
);
```

Inside the flush loop, before the existing `output.send_and_record(...)` call, capture the batch number:

```rust
while let Some(next_batch) = buffer.remove(&next_expected_batch_number) {
    let batch_num = next_batch.batch_number();
    next_expected_batch_number += 1;
    if output
        .send_and_record(next_batch, &self.health_reporter)
        .is_err()
    {
        anyhow::bail!("Outbound channel closed");
    }
    self.health_reporter.record_batch_number(batch_num);
}
```

- [ ] **Step 2: `gapless_l1_proof_sender.rs` — record_picked + record_batch_number**

After `buffer.insert(command.first_batch_number(), command)`:

```rust
match input.recv().await {
    Some(command) => {
        self.health_reporter.enter_state(GenericComponentState::Active);
        // record_picked: command arrived, may sit in reorder buffer before forwarding
        // L1SenderCommand needs first_batch_number() to identify the batch.
        // Use block number from HasBlockRangeEnd for the block-space coordinate:
        self.health_reporter.record_picked(
            command.block_number(),   // from HasBlockRangeEnd
            command.block_timestamp(),
        );
        buffer.insert(command.first_batch_number(), command);

        while let Some(next_command) = buffer.remove(&next_expected_batch_number) {
            let batch_num = next_expected_batch_number;
            next_expected_batch_number += next_command.batch_count() as u64;
            if output
                .send_and_record(next_command, &self.health_reporter)
                .is_err()
            {
                anyhow::bail!("Outbound channel closed");
            }
            self.health_reporter.record_batch_number(batch_num);
            self.health_reporter.enter_state(GenericComponentState::Active);
        }
    }
```

- [ ] **Step 3: `l1_sender/src/lib.rs` — add `record_picked` at receive**

`L1Sender` receives batches from its input channel. Locate the recv call and add `record_picked` immediately after:

```rust
// Find the pattern where L1Sender receives a command. It calls record_processed
// after the L1 transaction mines. Add record_picked at receive time.
// The exact lines depend on L1Sender's receive loop structure — find the recv
// and insert immediately after:
self.health_reporter.record_picked(last_block, last_block_timestamp);
```

Where `last_block` and `last_block_timestamp` are already captured from the received command (they're used for the existing `record_processed` call lower in the same function).

- [ ] **Step 4: Build the full binary**

```bash
cargo build --bin node 2>&1 | grep "^error" | head -20
```

Expected: clean build.

- [ ] **Step 5: Run all unit tests**

```bash
cargo nextest run --workspace --exclude zksync_os_integration_tests 2>&1 | tail -30
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add node/bin/src/prover_api/gapless_committer.rs \
        node/bin/src/prover_api/gapless_l1_proof_sender.rs \
        lib/l1_sender/src/lib.rs
git commit -m "feat: wire record_picked + record_batch_number for GaplessCommitter, GaplessL1ProofSender, L1Sender"
```

---

### Task 11: Integration test and final verification

**Files:**
- Modify: `integration-tests/tests/backpressure.rs`

- [ ] **Step 1: Update field references in `backpressure.rs`**

Search for any references to old field names:

```bash
grep -n "last_processed_block\|last_processed_block_number\|last_processed_block_timestamp" \
    integration-tests/tests/backpressure.rs
```

Replace each:
- `component.last_processed_block` → `component.last_processed_block.unwrap_or(0)` (now `Option`)
- Any direct struct-field access to adapt to the new `ComponentSnapshot` shape

- [ ] **Step 2: Add assertion that FRI lag does not falsely spike**

In the phase where batches are in-flight during the backpressure test, add an assertion that verifies `adjacent_block_lag` for FriJobManager reflects channel occupancy (is small or zero) rather than total proving lag. Find where the test polls for pipeline status and add:

```rust
// If FriJobManager has batches in-flight, adjacent_block_lag should be 0
// (channel ahead of it is empty — it has picked everything upstream sent).
if let Some(fri) = body["components"]
    .as_array()
    .and_then(|cs| cs.iter().find(|c| c["name"] == "fri_job_manager"))
{
    if fri["in_flight_first"].is_object() {
        // In-flight means it has picked from the channel — lag should be 0
        let adjacent_lag = fri["adjacent_block_lag"].as_u64().unwrap_or(0);
        assert_eq!(adjacent_lag, 0,
            "FriJobManager adjacent_block_lag should be 0 when batches are in-flight");
    }
}
```

- [ ] **Step 3: Run integration tests**

```bash
cargo nextest run -p zksync_os_integration_tests --profile no-pig 2>&1 | tail -40
```

Expected: all integration tests pass.

- [ ] **Step 4: Final build + unit test pass**

```bash
cargo build --release 2>&1 | grep "^error" | head -10
cargo nextest run --workspace --exclude zksync_os_integration_tests 2>&1 | tail -10
```

Expected: clean release build, all unit tests green.

- [ ] **Step 5: Commit**

```bash
git add integration-tests/tests/backpressure.rs
git commit -m "test: update backpressure integration test for new ComponentSnapshot shape

Field references updated to last_picked_block / last_processed_block.
New assertion: FriJobManager adjacent_block_lag is 0 when batches are
in-flight, validating the core correctness fix."
```

---

## Self-Review Notes

- **Spec coverage:** All five spec goals covered. (1) picked/processed split: Tasks 1–5. (2) Adjacent lag formula: Task 3. (3) In-flight range: Tasks 1, 6, 7, 8. (4) Batch coordinates: Tasks 1, 7–10. (5) No compat shims: Task 1 removes old fields entirely.
- **Type consistency:** `BatchTrackingCoordinates::new(batch_number, last_block_number, timestamp)` used uniformly in Tasks 6–10. `BlockTrackingCoordinates::new(block_number, timestamp)` used in Task 1.
- **`HasBlockRangeEnd` on `BlockCommand`:** Task 5 notes to confirm this. If `BlockCommand` doesn't implement it, extract `block_number` from the enum arms directly.
- **`L1SenderCommand::block_number()`:** Task 10 assumes `HasBlockRangeEnd` is implemented. Verify at build time — if not, use the already-captured `last_block` variable pattern from the existing `record_processed` call.
