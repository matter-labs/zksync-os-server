use crate::generic_component_state::GenericComponentState;
use crate::state_label::StateLabel;
use tokio::{sync::watch, time::Instant};

/// Block-space coordinates: block number and optional timestamp.
#[derive(Clone, Debug)]
pub struct BlockTrackingCoordinates {
    pub block_number: u64,
    pub timestamp: Option<u64>,
}

impl BlockTrackingCoordinates {
    pub fn new(block_number: u64, timestamp: Option<u64>) -> Self {
        Self {
            block_number,
            timestamp,
        }
    }
}

/// Batch-space coordinates for range-processing components (FriJobManager,
/// SnarkJobManager). Carries batch number alongside the batch's last block
/// number and timestamp so operators can identify in-flight batches directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchTrackingCoordinates {
    pub batch_number: u64,
    pub last_block_number: u64,
    pub timestamp: Option<u64>,
}

impl BatchTrackingCoordinates {
    pub fn new(batch_number: u64, last_block_number: u64, timestamp: Option<u64>) -> Self {
        Self {
            batch_number,
            last_block_number,
            timestamp,
        }
    }
}

/// State snapshot reported by a pipeline component on every state transition.
#[derive(Clone, Debug)]
pub struct ComponentState {
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

    /// Oldest batch currently in-flight. Populated by components that hold
    /// multiple batches concurrently with non-sequential completion:
    /// FriJobManager, SnarkJobManager (external provers), and the L1 senders
    /// (commit/prove/execute — parallel L1 transactions awaiting inclusion).
    pub in_flight_first: Option<BatchTrackingCoordinates>,

    /// Newest batch currently in-flight. See `in_flight_first` for producers.
    pub in_flight_last: Option<BatchTrackingCoordinates>,

    /// Last batch number fully processed by this component.
    /// Only populated for batch-pipeline components (Batcher and downstream).
    /// High-watermark semantics.
    pub batch_number: Option<u64>,
    /// Last batch number dequeued from the input channel by this component.
    /// High-watermark semantics. Absent until first batch received.
    /// Only populated for batch-pipeline components that receive batches from upstream.
    pub last_batch_picked: Option<u64>,
}

/// Uses `watch::Sender` — updates are infallible, no background task, no global state.
#[derive(Debug)]
pub struct ComponentStateReporter {
    sender: watch::Sender<ComponentState>,
    component: &'static str,
}

impl ComponentStateReporter {
    /// Returns the reporter (owned by the component) and the receiver (handed to the monitor).
    pub fn new(component: &'static str) -> (Self, watch::Receiver<ComponentState>) {
        let initial = ComponentState {
            state: GenericComponentState::Idle,
            specific_state: "idle",
            state_entered_at: Instant::now(),
            last_picked: None,
            last_processed: None,
            in_flight_first: None,
            in_flight_last: None,
            batch_number: None,
            last_batch_picked: None,
        };
        let (sender, receiver) = watch::channel(initial);
        (Self { sender, component }, receiver)
    }

    /// Reporter for auxiliary tasks that emit `component_time_spent_in_state`
    /// metrics but are not wired into any pipeline monitor. The watch receiver
    /// is deliberately dropped so the component can never be mistaken for a
    /// pipeline stage whose progress should be observed for adjacency lag or
    /// backpressure. Prefer this over `new(..).0` when you know the task is
    /// metrics-only by design.
    pub fn unmonitored(component: &'static str) -> Self {
        let (reporter, _rx) = Self::new(component);
        reporter
    }

    /// Transition to a new state and record time-in-previous-state metric.
    pub fn enter_state(&self, new_state: impl StateLabel) {
        let now = Instant::now();
        self.sender.send_modify(|state| {
            if state.specific_state == new_state.specific() {
                return;
            }
            let elapsed = now.duration_since(state.state_entered_at);
            crate::metrics::GENERAL_METRICS.component_time_spent_in_state
                [&(self.component, state.state, state.specific_state)]
                .inc_by(elapsed.as_secs_f64());
            state.state = new_state.generic();
            state.specific_state = new_state.specific();
            state.state_entered_at = now;
        });
    }

    /// Record when an item was dequeued from the input channel (before any processing).
    /// `batch_number` is `None` for block-pipeline components and `Some` for batch-pipeline
    /// components — both the block-space and batch-space watermarks update atomically under
    /// a single `send_if_modified`, so the monitor wakes once per pick instead of twice.
    /// High-watermark semantics: stale out-of-order calls are ignored per dimension.
    pub fn record_picked(
        &self,
        block_number: u64,
        timestamp: Option<u64>,
        batch_number: Option<u64>,
    ) {
        self.sender.send_if_modified(|state| {
            let mut modified = false;
            let block_stale = state
                .last_picked
                .as_ref()
                .is_some_and(|c| block_number < c.block_number);
            if !block_stale {
                state.last_picked = Some(BlockTrackingCoordinates::new(block_number, timestamp));
                modified = true;
            }
            if let Some(bn) = batch_number {
                let batch_stale = state.last_batch_picked.is_some_and(|c| bn < c);
                if !batch_stale {
                    state.last_batch_picked = Some(bn);
                    modified = true;
                }
            }
            modified
        });
    }

    /// Record when an item was fully handled/forwarded downstream.
    /// `batch_number` is `None` for block-pipeline components and `Some` for batch-pipeline
    /// components. Both dimensions update atomically under a single `send_if_modified`.
    /// High-watermark semantics: stale out-of-order calls are ignored per dimension.
    pub fn record_processed(
        &self,
        block_number: u64,
        timestamp: Option<u64>,
        batch_number: Option<u64>,
    ) {
        self.sender.send_if_modified(|state| {
            let mut modified = false;
            let block_stale = state
                .last_processed
                .as_ref()
                .is_some_and(|c| block_number < c.block_number);
            if !block_stale {
                state.last_processed = Some(BlockTrackingCoordinates::new(block_number, timestamp));
                modified = true;
            }
            if let Some(bn) = batch_number {
                let batch_stale = state.batch_number.is_some_and(|c| bn < c);
                if !batch_stale {
                    state.batch_number = Some(bn);
                    modified = true;
                }
            }
            modified
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
        if let (Some(f), Some(l)) = (first.as_ref(), last.as_ref()) {
            debug_assert!(
                f.batch_number <= l.batch_number,
                "record_in_flight_range: first ({}) must be <= last ({})",
                f.batch_number,
                l.batch_number,
            );
        }
        self.sender.send_if_modified(|state| {
            if state.in_flight_first == first && state.in_flight_last == last {
                return false;
            }
            state.in_flight_first = first;
            state.in_flight_last = last;
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
        let (reporter, rx) = ComponentStateReporter::new("test_component");
        let state = rx.borrow().clone();
        assert_eq!(state.state, GenericComponentState::Idle);
        assert_eq!(state.specific_state, "idle");
        assert!(state.last_picked.is_none());
        assert!(state.last_processed.is_none());
        drop(reporter);
    }

    #[tokio::test]
    async fn enter_state_updates_receiver() {
        let (reporter, rx) = ComponentStateReporter::new("test_component");
        reporter.enter_state(GenericComponentState::Active);
        assert_eq!(rx.borrow().state, GenericComponentState::Active);
    }

    #[tokio::test]
    async fn record_processed_updates_coord() {
        let (reporter, rx) = ComponentStateReporter::new("test_component");
        reporter.record_processed(42, Some(1_700_000_000), None);
        let h = rx.borrow();
        let coord = h.last_processed.as_ref().unwrap();
        assert_eq!(coord.block_number, 42);
        assert_eq!(coord.timestamp, Some(1_700_000_000));
    }

    #[tokio::test]
    async fn record_processed_high_watermark() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_processed(100, Some(1_000), None);
        reporter.record_processed(80, Some(800), None); // stale
        assert_eq!(
            rx.borrow().last_processed.as_ref().unwrap().block_number,
            100
        );
        assert_eq!(
            rx.borrow().last_processed.as_ref().unwrap().timestamp,
            Some(1_000)
        );
    }

    #[tokio::test]
    async fn record_processed_accepts_equal_block_number() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_processed(50, Some(500), None);
        reporter.record_processed(50, Some(501), None);
        assert_eq!(
            rx.borrow().last_processed.as_ref().unwrap().timestamp,
            Some(501)
        );
    }

    #[tokio::test]
    async fn state_entered_at_updates_on_enter_state() {
        let (reporter, rx) = ComponentStateReporter::new("test_component");
        let t0 = rx.borrow().state_entered_at;
        sleep(Duration::from_millis(10)).await;
        reporter.enter_state(GenericComponentState::Active);
        assert!(rx.borrow().state_entered_at > t0);
    }

    #[tokio::test]
    async fn enter_state_same_state_does_not_reset_timer() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        let t0 = rx.borrow().state_entered_at;
        tokio::time::sleep(Duration::from_millis(10)).await;
        reporter.enter_state(GenericComponentState::Idle);
        assert_eq!(rx.borrow().state_entered_at, t0);
    }

    #[tokio::test]
    async fn multiple_reporters_independent() {
        let (r1, rx1) = ComponentStateReporter::new("c1");
        let (r2, rx2) = ComponentStateReporter::new("c2");
        r1.record_processed(10, None, None);
        r2.record_processed(20, None, None);
        assert_eq!(
            rx1.borrow().last_processed.as_ref().unwrap().block_number,
            10
        );
        assert_eq!(
            rx2.borrow().last_processed.as_ref().unwrap().block_number,
            20
        );
    }

    #[tokio::test]
    async fn record_picked_advances_independently_of_processed() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_picked(5, Some(500), None);
        reporter.record_processed(3, Some(300), None);
        let h = rx.borrow();
        assert_eq!(h.last_picked.as_ref().unwrap().block_number, 5);
        assert_eq!(h.last_processed.as_ref().unwrap().block_number, 3);
    }

    #[tokio::test]
    async fn record_picked_high_watermark_guard() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_picked(10, None, None);
        reporter.record_picked(5, None, None);
        assert_eq!(rx.borrow().last_picked.as_ref().unwrap().block_number, 10);
    }

    #[tokio::test]
    async fn record_in_flight_range_stores_both_ends() {
        let (reporter, rx) = ComponentStateReporter::new("test");
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
        let (reporter, rx) = ComponentStateReporter::new("test");
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
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_processed(0, None, Some(10));
        reporter.record_processed(0, None, Some(5));
        assert_eq!(rx.borrow().batch_number, Some(10));
    }

    #[tokio::test]
    async fn record_processed_updates_both_block_and_batch_atomically() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_processed(42, Some(999), Some(7));
        let h = rx.borrow();
        let coord = h.last_processed.as_ref().unwrap();
        assert_eq!(coord.block_number, 42);
        assert_eq!(coord.timestamp, Some(999));
        assert_eq!(h.batch_number, Some(7));
    }

    #[tokio::test]
    async fn record_picked_updates_both_block_and_batch_atomically() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_picked(42, Some(999), Some(7));
        let h = rx.borrow();
        let coord = h.last_picked.as_ref().unwrap();
        assert_eq!(coord.block_number, 42);
        assert_eq!(coord.timestamp, Some(999));
        assert_eq!(h.last_batch_picked, Some(7));
    }
}
