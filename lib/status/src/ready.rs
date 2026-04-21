use crate::AppState;
use axum::{extract::State, http::StatusCode};

pub(crate) async fn ready(State(state): State<AppState>) -> StatusCode {
    if *state.stop_receiver.borrow() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::watch;
    use zksync_os_backpressure::{BackpressureConfig, ComponentId};
    use zksync_os_observability::ComponentStateReporter;
    use zksync_os_types::TransactionAcceptanceState;

    fn make_state(stop: bool, acceptance: TransactionAcceptanceState) -> AppState {
        let (_stop_tx, stop_rx) = watch::channel(stop);
        let (_accept_tx, accept_rx) = watch::channel(acceptance);
        let (reporter, state_rx) = ComponentStateReporter::new("block_executor");
        reporter.record_processed(0, None, None);
        AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_states: Arc::new(vec![(ComponentId::BlockExecutor, state_rx)]),
            edges: Arc::new(vec![]),
            backpressure_config: BackpressureConfig::default(),
        }
    }

    #[tokio::test]
    async fn returns_200_when_not_shutting_down() {
        let state = make_state(false, TransactionAcceptanceState::Accepting);
        assert_eq!(ready(State(state)).await, StatusCode::OK);
    }

    /// Readiness must stay 200 when the node is not accepting transactions: RPC readers in
    /// this process keep serving during `NotAccepting`, so K8s readiness must not drain the
    /// pod from service endpoints on a transient acceptance flip. Acceptance state is
    /// surfaced separately by `/status/accepting`.
    #[tokio::test]
    async fn returns_200_even_when_not_accepting() {
        use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};
        let cause = BackpressureCause {
            component: "fri_job_manager",
            trigger: BackpressureTrigger::BlockDiffToUpstreamTooHigh {
                threshold: 500,
                actual: 782,
            },
        };
        let state = make_state(
            false,
            TransactionAcceptanceState::NotAccepting(vec![
                NotAcceptingReason::PipelineBackpressure {
                    causes: vec![cause],
                },
            ]),
        );
        assert_eq!(ready(State(state)).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn returns_503_during_graceful_shutdown() {
        let state = make_state(true, TransactionAcceptanceState::Accepting);
        assert_eq!(ready(State(state)).await, StatusCode::SERVICE_UNAVAILABLE);
    }
}
