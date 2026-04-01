use crate::generic_component_state::GenericComponentState;
use crate::state_label::StateLabel;
use tokio::{sync::watch, time::Instant};

/// Health snapshot reported by a pipeline component on every state transition.
#[derive(Clone, Debug)]
pub struct ComponentHealth {
    pub state: GenericComponentState,
    /// Fine-grained state string from the component's StateLabel impl.
    pub specific_state: &'static str,
    /// When the current state was entered (monotonic). This is [`tokio::time::Instant`],
    /// not `std::time::Instant`; callers computing durations must pair it with
    /// `tokio::time::Instant::now()`.
    pub state_entered_at: Instant,
    /// Block number of the last item successfully processed. `None` until first call to
    /// `record_processed`.
    pub last_processed_block_number: Option<u64>,
    /// Block timestamp of the last processed block. `None` if not yet processed or unavailable
    /// (e.g. batch-level components that call `record_processed` with `None`).
    pub last_processed_block_timestamp: Option<u64>,
    /// When record_processed was last called (None until first call).
    /// Independent from state_entered_at — tracks processing rate, not state duration.
    /// This is [`tokio::time::Instant`]; pair with `tokio::time::Instant::now()` for durations.
    pub last_processed_block_at: Option<Instant>,
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
            last_processed_block_number: None,
            last_processed_block_timestamp: None,
            last_processed_block_at: None,
        };
        let (sender, receiver) = watch::channel(initial);
        (Self { sender, component }, receiver)
    }

    /// Transition to a new state and record time-in-previous-state metric.
    pub fn enter_state(&self, new_state: impl StateLabel) {
        let now = Instant::now();
        self.sender.send_modify(|health| {
            // No-op: don't reset timer if transitioning to the same state.
            if health.specific_state == new_state.specific() {
                return;
            }
            let elapsed = now.duration_since(health.state_entered_at);
            // Credit elapsed time to the OLD state (the one we are leaving).
            crate::metrics::GENERAL_METRICS.component_time_spent_in_state
                [&(self.component, health.state, health.specific_state)]
                .inc_by(elapsed.as_secs_f64());
            health.state = new_state.generic();
            health.specific_state = new_state.specific();
            health.state_entered_at = now;
        });
    }

    /// Record the block number and timestamp of the last item successfully processed.
    /// Use `block_timestamp = None` for batch-level components where block timestamps
    /// are not readily available. Time-lag evaluation is skipped for those.
    ///
    /// High-watermark semantics: if `block_number` is less than the currently stored
    /// maximum, the call is a no-op. This prevents concurrent reporters (e.g. parallel
    /// provers) from walking the watermark backward when an older batch finishes after
    /// a newer one.
    pub fn record_processed(&self, block_number: u64, block_timestamp: Option<u64>) {
        let now = Instant::now();
        self.sender.send_if_modified(|health| {
            // High-watermark guard: ignore stale out-of-order reports.
            if let Some(current_max) = health.last_processed_block_number
                && block_number < current_max
            {
                return false;
            }
            health.last_processed_block_number = Some(block_number);
            health.last_processed_block_timestamp = block_timestamp;
            health.last_processed_block_at = Some(now);
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
        assert_eq!(health.last_processed_block_number, None);
        assert_eq!(health.last_processed_block_timestamp, None);
        drop(reporter);
    }

    #[tokio::test]
    async fn enter_state_updates_receiver() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        reporter.enter_state(GenericComponentState::Active);
        let health = rx.borrow().clone();
        assert_eq!(health.state, GenericComponentState::Active);
    }

    #[tokio::test]
    async fn enter_state_records_specific_state() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.enter_state(GenericComponentState::Active);
        let health = rx.borrow().clone();
        assert_eq!(health.state, GenericComponentState::Active);
        assert_eq!(health.specific_state, "active");
    }

    #[tokio::test]
    async fn record_processed_updates_seq_and_timestamp() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        reporter.record_processed(42, Some(1_700_000_000));
        assert_eq!(rx.borrow().last_processed_block_number, Some(42));
        assert_eq!(
            rx.borrow().last_processed_block_timestamp,
            Some(1_700_000_000)
        );
        reporter.record_processed(100, Some(1_700_000_100));
        assert_eq!(rx.borrow().last_processed_block_number, Some(100));
        assert_eq!(
            rx.borrow().last_processed_block_timestamp,
            Some(1_700_000_100)
        );
    }

    #[tokio::test]
    async fn state_entered_at_updates_on_enter_state() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        let t0 = rx.borrow().state_entered_at;
        sleep(Duration::from_millis(10)).await;
        reporter.enter_state(GenericComponentState::Active);
        let t1 = rx.borrow().state_entered_at;
        assert!(t1 > t0, "state_entered_at must advance");
    }

    #[tokio::test]
    async fn multiple_reporters_independent() {
        let (r1, rx1) = ComponentHealthReporter::new("c1");
        let (r2, rx2) = ComponentHealthReporter::new("c2");
        r1.record_processed(10, None);
        r2.record_processed(20, None);
        assert_eq!(rx1.borrow().last_processed_block_number, Some(10));
        assert_eq!(rx2.borrow().last_processed_block_number, Some(20));
    }

    #[tokio::test]
    async fn enter_state_same_state_does_not_reset_timer() {
        // NOTE: We must sleep before calling enter_state so that a real timer reset
        // would produce t1 > t0. Without the sleep, t0 and t1 could be equal even
        // if the guard is absent (timer resolution), giving a false green.
        let (reporter, rx) = ComponentHealthReporter::new("test");
        let t0 = rx.borrow().state_entered_at;
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Entering the same state (Idle→Idle) must not advance state_entered_at.
        reporter.enter_state(GenericComponentState::Idle);
        let t1 = rx.borrow().state_entered_at;
        assert_eq!(
            t0, t1,
            "state_entered_at must not change for same-state transition"
        );
    }

    #[tokio::test]
    async fn record_processed_updates_last_processed_block_at() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        assert!(rx.borrow().last_processed_block_at.is_none());
        reporter.record_processed(1, None);
        assert!(rx.borrow().last_processed_block_at.is_some());
    }

    #[tokio::test]
    async fn record_processed_ignores_lower_block_number() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_processed(100, Some(1_000));
        let at_before = rx.borrow().last_processed_block_at;
        reporter.record_processed(80, Some(800));
        assert_eq!(
            rx.borrow().last_processed_block_number,
            Some(100),
            "block number must not regress"
        );
        assert_eq!(
            rx.borrow().last_processed_block_timestamp,
            Some(1_000),
            "timestamp must stay with the highest block"
        );
        assert_eq!(
            rx.borrow().last_processed_block_at,
            at_before,
            "last_processed_block_at must not update on stale out-of-order report"
        );
    }

    #[tokio::test]
    async fn record_processed_accepts_equal_block_number() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_processed(50, Some(500));
        reporter.record_processed(50, Some(501));
        assert_eq!(rx.borrow().last_processed_block_number, Some(50));
        assert_eq!(rx.borrow().last_processed_block_timestamp, Some(501));
    }

    #[tokio::test]
    async fn record_processed_advances_past_max() {
        let (reporter, rx) = ComponentHealthReporter::new("test");
        reporter.record_processed(50, Some(500));
        reporter.record_processed(60, Some(600));
        assert_eq!(rx.borrow().last_processed_block_number, Some(60));
        assert_eq!(rx.borrow().last_processed_block_timestamp, Some(600));
    }
}
