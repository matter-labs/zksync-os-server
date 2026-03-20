use crate::generic_component_state::GenericComponentState;
use tokio::{sync::watch, time::Instant};

/// Health snapshot reported by a pipeline component on every state transition.
#[derive(Clone, Debug)]
pub struct ComponentHealth {
    pub state: GenericComponentState,
    /// When the current state was entered (monotonic).
    pub state_entered_at: Instant,
    /// Block number of the last item successfully sent downstream.
    pub last_processed_seq: u64,
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
            state: GenericComponentState::WaitingRecv,
            state_entered_at: Instant::now(),
            last_processed_seq: 0,
        };
        let (sender, receiver) = watch::channel(initial);
        (Self { sender, component }, receiver)
    }

    /// Transition to a new state and record time-in-previous-state metric.
    pub fn enter_state(&self, new_state: GenericComponentState) {
        let now = Instant::now();
        self.sender.send_modify(|health| {
            let elapsed = now.duration_since(health.state_entered_at);
            // GENERAL_METRICS.component_time_spent_in_state uses Counter<f64> with inc_by.
            crate::metrics::GENERAL_METRICS.component_time_spent_in_state
                [&(self.component, health.state, health.state.specific())]
                .inc_by(elapsed.as_secs_f64());
            health.state = new_state;
            health.state_entered_at = now;
        });
    }

    /// Record the block number of the last item successfully sent downstream.
    pub fn record_processed(&self, block_seq: u64) {
        self.sender.send_modify(|health| {
            health.last_processed_seq = block_seq;
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
    async fn reporter_new_starts_in_waiting_recv() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        let health = rx.borrow().clone();
        assert_eq!(health.state, GenericComponentState::WaitingRecv);
        assert_eq!(health.last_processed_seq, 0);
        drop(reporter);
    }

    #[tokio::test]
    async fn enter_state_updates_receiver() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        reporter.enter_state(GenericComponentState::Processing);
        let health = rx.borrow().clone();
        assert_eq!(health.state, GenericComponentState::Processing);
    }

    #[tokio::test]
    async fn record_processed_updates_seq() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        reporter.record_processed(42);
        assert_eq!(rx.borrow().last_processed_seq, 42);
        reporter.record_processed(100);
        assert_eq!(rx.borrow().last_processed_seq, 100);
    }

    #[tokio::test]
    async fn state_entered_at_updates_on_enter_state() {
        let (reporter, rx) = ComponentHealthReporter::new("test_component");
        let t0 = rx.borrow().state_entered_at;
        sleep(Duration::from_millis(10)).await;
        reporter.enter_state(GenericComponentState::Processing);
        let t1 = rx.borrow().state_entered_at;
        assert!(t1 > t0, "state_entered_at must advance");
    }

    #[tokio::test]
    async fn multiple_reporters_independent() {
        let (r1, rx1) = ComponentHealthReporter::new("c1");
        let (r2, rx2) = ComponentHealthReporter::new("c2");
        r1.record_processed(10);
        r2.record_processed(20);
        assert_eq!(rx1.borrow().last_processed_seq, 10);
        assert_eq!(rx2.borrow().last_processed_seq, 20);
    }
}
