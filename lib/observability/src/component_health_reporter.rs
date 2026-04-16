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
#[derive(Clone, Debug)]
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
    /// Last batch number dequeued from the input channel by this component.
    /// High-watermark semantics. Absent until first batch received.
    /// Only populated for batch-pipeline components that receive batches from upstream.
    pub last_batch_picked: Option<u64>,
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
            last_batch_picked: None,
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
            if let Some(ref current) = health.last_picked
                && block_number < current.block_number
            {
                return false;
            }
            health.last_picked = Some(BlockTrackingCoordinates::new(block_number, timestamp));
            true
        });
    }

    /// Record when a block was fully handled/forwarded downstream.
    /// High-watermark semantics: stale out-of-order calls are ignored.
    pub fn record_processed(&self, block_number: u64, timestamp: Option<u64>) {
        self.sender.send_if_modified(|health| {
            if let Some(ref current) = health.last_processed
                && block_number < current.block_number
            {
                return false;
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

    /// Record when a batch was dequeued from the input channel (before any processing).
    /// High-watermark semantics: stale out-of-order calls are ignored.
    pub fn record_batch_picked(&self, batch_number: u64) {
        self.sender.send_if_modified(|health| {
            if let Some(current) = health.last_batch_picked
                && batch_number < current
            {
                return false;
            }
            health.last_batch_picked = Some(batch_number);
            true
        });
    }

    /// Record the last completed batch number for batch-pipeline components.
    /// High-watermark semantics: stale out-of-order calls are ignored.
    pub fn record_batch_number(&self, batch_number: u64) {
        self.sender.send_if_modified(|health| {
            if let Some(current) = health.batch_number
                && batch_number < current
            {
                return false;
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
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_processed(50, Some(500));
        reporter.record_processed(50, Some(501));
        assert_eq!(
            rx.borrow().last_processed.as_ref().unwrap().timestamp,
            Some(501)
        );
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
        assert_eq!(
            rx1.borrow().last_processed.as_ref().unwrap().block_number,
            10
        );
        assert_eq!(
            rx2.borrow().last_processed.as_ref().unwrap().block_number,
            20
        );
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
