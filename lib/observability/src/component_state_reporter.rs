use crate::generic_component_state::GenericComponentState;
use crate::metrics::GENERAL_METRICS;
use crate::state_label::StateLabel;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// Coordinates for a pipeline item
#[derive(Clone, Debug)]
pub struct TrackingCoordinates {
    pub block_number: u64,
    pub timestamp: Option<u64>,
    pub batch_number: Option<u64>,
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

    /// Last item picked from the input channel.
    pub picked: Option<TrackingCoordinates>,

    /// Last item fully handled/forwarded downstream.
    pub processed: Option<TrackingCoordinates>,
}

#[derive(Debug, Clone)]
pub struct ComponentStateReporter {
    sender: watch::Sender<ComponentState>,
    state_tx: mpsc::Sender<(GenericComponentState, &'static str)>,
}

impl ComponentStateReporter {
    /// Returns the reporter (owned by the component) and the receiver (handed to the monitor).
    pub fn new(component: &'static str) -> (Self, watch::Receiver<ComponentState>) {
        let initial = ComponentState {
            state: GenericComponentState::Idle,
            specific_state: "idle",
            state_entered_at: Instant::now(),
            picked: None,
            processed: None,
        };
        let (sender, receiver) = watch::channel(initial);
        let (state_tx, state_rx) = mpsc::channel(512);
        tokio::spawn(flush_state_time(
            component,
            state_rx,
            GenericComponentState::Idle,
            "idle",
        ));
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
            let _ = self.state_tx.try_send((new_generic, new_specific));
        }
    }

    /// Record when an item was dequeued from the input channel (before any processing).
    pub fn record_picked(
        &self,
        block_number: u64,
        timestamp: Option<u64>,
        batch_number: Option<u64>,
    ) {
        self.sender.send_if_modified(|state| {
            let stale = state
                .picked
                .as_ref()
                .is_some_and(|c| block_number < c.block_number);
            if stale {
                return false;
            }
            state.picked = Some(TrackingCoordinates {
                block_number,
                timestamp,
                batch_number,
            });
            true
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
            let stale = state
                .processed
                .as_ref()
                .is_some_and(|c| block_number < c.block_number);
            if stale {
                return false;
            }
            state.processed = Some(TrackingCoordinates {
                block_number,
                timestamp,
                batch_number,
            });
            true
        });
    }
}

/// Runs as a per-component background task. Continuously increments
/// `component_time_spent_in_state` on every 2-second tick and on every state.
async fn flush_state_time(
    component: &'static str,
    mut rx: mpsc::Receiver<(GenericComponentState, &'static str)>,
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
        assert_eq!(rx.borrow().processed.as_ref().unwrap().block_number, 100);
        assert_eq!(
            rx.borrow().processed.as_ref().unwrap().timestamp,
            Some(1_000)
        );
    }

    #[tokio::test]
    async fn record_processed_accepts_equal_block_number() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_processed(50, Some(500), None);
        reporter.record_processed(50, Some(501), None);
        assert_eq!(rx.borrow().processed.as_ref().unwrap().timestamp, Some(501));
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
        assert_eq!(h.picked.as_ref().unwrap().block_number, 5);
        assert_eq!(h.processed.as_ref().unwrap().block_number, 3);
    }

    #[tokio::test]
    async fn record_picked_high_watermark_guard() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_picked(10, None, None);
        reporter.record_picked(5, None, None);
        assert_eq!(rx.borrow().picked.as_ref().unwrap().block_number, 10);
    }

    #[tokio::test]
    async fn record_batch_number_high_watermark() {
        let (reporter, rx) = ComponentStateReporter::new("test");
        reporter.record_processed(100, None, Some(10));
        reporter.record_processed(50, None, Some(5)); // stale on block_number
        assert_eq!(
            rx.borrow().processed.as_ref().and_then(|c| c.batch_number),
            Some(10)
        );
    }
}
