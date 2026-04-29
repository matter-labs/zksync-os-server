use crate::generic_component_state::GenericComponentState;
use crate::metrics::GENERAL_METRICS;
use crate::state_label::StateLabel;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// Block-space coordinates.
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

/// Batch-space coordinates.
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
    /// Component state - Idle or Active.
    pub state: GenericComponentState,

    /// Fine-grained state label.
    pub specific_state: &'static str,

    /// When the current state was entered.
    pub state_entered_at: Instant,

    /// When this component last dequeued an item from its input channel.
    /// Absent until the first item is received. High-watermark semantics.
    pub block_picked: Option<BlockTrackingCoordinates>,

    /// When this component last fully handled/forwarded an item downstream.
    /// Absent until the first item is fully processed. High-watermark semantics.
    pub block_processed: Option<BlockTrackingCoordinates>,

    /// Last batch number dequeued from the input channel by this component.
    pub batch_picked: Option<u64>,

    /// Last batch number fully processed.
    pub batch_processed: Option<u64>,

    /// Oldest batch currently in-flight.
    pub in_flight_first_batch: Option<BatchTrackingCoordinates>,

    /// Newest batch currently in-flight.
    pub in_flight_last_batch: Option<BatchTrackingCoordinates>,
}

#[derive(Debug, Clone)]
pub struct ComponentStateReporter {
    sender: watch::Sender<ComponentState>,
    state_tx: mpsc::UnboundedSender<(GenericComponentState, &'static str)>,
}

impl ComponentStateReporter {
    /// Returns the reporter (owned by the component) and the receiver (handed to the monitor).
    pub fn new(component: &'static str) -> (Self, watch::Receiver<ComponentState>) {
        let initial = ComponentState {
            state: GenericComponentState::Idle,
            specific_state: "idle",
            state_entered_at: Instant::now(),
            block_picked: None,
            block_processed: None,
            in_flight_first_batch: None,
            in_flight_last_batch: None,
            batch_processed: None,
            batch_picked: None,
        };
        let (sender, receiver) = watch::channel(initial);
        let (state_tx, state_rx) = mpsc::unbounded_channel();
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(flush_state_time(
                component,
                state_rx,
                GenericComponentState::Idle,
                "idle",
            ));
        }
        (Self { sender, state_tx }, receiver)
    }

    /// Transition to a new state.
    pub fn enter_state(&self, new_state: impl StateLabel) {
        let now = Instant::now();
        let new_generic = new_state.generic();
        let new_specific = new_state.specific();
        let mut transitioned = false;
        self.sender.send_modify(|state| {
            if state.specific_state == new_specific {
                return;
            }
            transitioned = true;
            state.state = new_generic;
            state.specific_state = new_specific;
            state.state_entered_at = now;
        });
        if transitioned {
            let _ = self.state_tx.send((new_generic, new_specific));
        }
    }

    /// Record when an item was dequeued from the input channel (before any processing)
    pub fn record_picked(
        &self,
        block_number: u64,
        timestamp: Option<u64>,
        batch_number: Option<u64>,
    ) {
        self.sender.send_if_modified(|state| {
            let mut modified = false;
            let block_stale = state
                .block_picked
                .as_ref()
                .is_some_and(|c| block_number < c.block_number);
            if !block_stale {
                state.block_picked = Some(BlockTrackingCoordinates::new(block_number, timestamp));
                modified = true;
            }
            if let Some(bn) = batch_number {
                let batch_stale = state.batch_picked.is_some_and(|c| bn < c);
                if !batch_stale {
                    state.batch_picked = Some(bn);
                    modified = true;
                }
            }
            modified
        });
    }

    /// Record when an item was fully processed.
    pub fn record_processed(
        &self,
        block_number: u64,
        timestamp: Option<u64>,
        batch_number: Option<u64>,
    ) {
        self.sender.send_if_modified(|state| {
            let mut modified = false;
            let block_stale = state
                .block_processed
                .as_ref()
                .is_some_and(|c| block_number < c.block_number);
            if !block_stale {
                state.block_processed =
                    Some(BlockTrackingCoordinates::new(block_number, timestamp));
                modified = true;
            }
            if let Some(bn) = batch_number {
                let batch_stale = state.batch_processed.is_some_and(|c| bn < c);
                if !batch_stale {
                    state.batch_processed = Some(bn);
                    modified = true;
                }
            }
            modified
        });
    }

    /// Record the current in-flight range for range-processing components.
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
            if state.in_flight_first_batch == first && state.in_flight_last_batch == last {
                return false;
            }
            state.in_flight_first_batch = first;
            state.in_flight_last_batch = last;
            true
        });
    }
}

/// Runs as a per-component background task. Continuously increments
/// `component_time_spent_in_state` on every 2-second tick and on every state.
async fn flush_state_time(
    component: &'static str,
    mut rx: mpsc::UnboundedReceiver<(GenericComponentState, &'static str)>,
    initial_state: GenericComponentState,
    initial_specific: &'static str,
) {
    const TICK: Duration = Duration::from_secs(2);
    let mut tracked_state = initial_state;
    let mut tracked_specific = initial_specific;
    let mut last_flush = Instant::now();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = Instant::now();
                let elapsed = now.duration_since(last_flush).as_secs_f64();
                if elapsed > 0.0 {
                    GENERAL_METRICS.component_time_spent_in_state
                        [&(component, tracked_state, tracked_specific)]
                        .inc_by(elapsed);
                }
                last_flush = now;
            }
            msg = rx.recv() => {
                let Some((new_state, new_specific)) = msg else { return };
                let now = Instant::now();
                let elapsed = now.duration_since(last_flush).as_secs_f64();
                if elapsed > 0.0 {
                    GENERAL_METRICS.component_time_spent_in_state
                        [&(component, tracked_state, tracked_specific)]
                        .inc_by(elapsed);
                }
                tracked_state = new_state;
                tracked_specific = new_specific;
                last_flush = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GenericComponentState;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn record_processed_high_watermark() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_processed(100, Some(1_000), None);
        reporter.record_processed(80, Some(800), None); // stale
        assert_eq!(
            rx.borrow().block_processed.as_ref().unwrap().block_number,
            100
        );
        assert_eq!(
            rx.borrow().block_processed.as_ref().unwrap().timestamp,
            Some(1_000)
        );
    }

    #[tokio::test]
    async fn record_processed_accepts_equal_block_number() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_processed(50, Some(500), None);
        reporter.record_processed(50, Some(501), None);
        assert_eq!(
            rx.borrow().block_processed.as_ref().unwrap().timestamp,
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
    async fn record_picked_advances_independently_of_processed() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_picked(5, Some(500), None);
        reporter.record_processed(3, Some(300), None);
        let h = rx.borrow();
        assert_eq!(h.block_picked.as_ref().unwrap().block_number, 5);
        assert_eq!(h.block_processed.as_ref().unwrap().block_number, 3);
    }

    #[tokio::test]
    async fn record_picked_high_watermark_guard() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_picked(10, None, None);
        reporter.record_picked(5, None, None);
        assert_eq!(rx.borrow().block_picked.as_ref().unwrap().block_number, 10);
    }

    #[tokio::test]
    async fn record_in_flight_range_stores_both_ends() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_in_flight_range(
            Some(BatchTrackingCoordinates::new(1, 100, Some(1000))),
            Some(BatchTrackingCoordinates::new(5, 500, Some(5000))),
        );
        let h = rx.borrow();
        assert_eq!(h.in_flight_first_batch.as_ref().unwrap().batch_number, 1);
        assert_eq!(h.in_flight_last_batch.as_ref().unwrap().batch_number, 5);
        assert_eq!(
            h.in_flight_first_batch.as_ref().unwrap().last_block_number,
            100
        );
        assert_eq!(
            h.in_flight_last_batch.as_ref().unwrap().last_block_number,
            500
        );
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
        assert!(h.in_flight_first_batch.is_none());
        assert!(h.in_flight_last_batch.is_none());
    }

    #[tokio::test]
    async fn record_batch_number_high_watermark() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_processed(0, None, Some(10));
        reporter.record_processed(0, None, Some(5));
        assert_eq!(rx.borrow().batch_processed, Some(10));
    }
}
