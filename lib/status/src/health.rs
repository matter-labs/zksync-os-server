use crate::AppState;
use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;
use zksync_os_types::{BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState};

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Healthy,
    Backpressure,
    Terminating,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: NodeStatus,
    pub accepting_transactions: bool,
    pub backpressure_causes: Vec<BackpressureCauseJson>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct BackpressureCauseJson {
    pub component: &'static str,
    pub trigger: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_blocks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_blocks: Option<u64>,
}

pub(crate) async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthResponse>) {
    let is_terminating = *state.stop_receiver.borrow();
    let acceptance = state.acceptance_state.borrow().clone();
    let accepting = matches!(acceptance, TransactionAcceptanceState::Accepting);

    let backpressure_causes = match &acceptance {
        TransactionAcceptanceState::NotAccepting(NotAcceptingReason::PipelineBackpressure {
            causes,
        }) => causes
            .iter()
            .map(|c| match &c.trigger {
                BackpressureTrigger::TimeLagTooHigh { threshold, actual } => {
                    BackpressureCauseJson {
                        component: c.component,
                        trigger: "time_lag_too_high",
                        threshold_secs: Some(threshold.as_secs_f64()),
                        actual_secs: Some(actual.as_secs_f64()),
                        threshold_blocks: None,
                        actual_blocks: None,
                    }
                }
                BackpressureTrigger::BlockLagTooHigh { threshold, actual } => {
                    BackpressureCauseJson {
                        component: c.component,
                        trigger: "block_lag_too_high",
                        threshold_secs: None,
                        actual_secs: None,
                        threshold_blocks: Some(*threshold),
                        actual_blocks: Some(*actual),
                    }
                }
            })
            .collect(),
        _ => vec![],
    };

    let node_status = if is_terminating {
        NodeStatus::Terminating
    } else if accepting {
        NodeStatus::Healthy
    } else {
        NodeStatus::Backpressure
    };

    let http_status = if matches!(node_status, NodeStatus::Healthy) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        http_status,
        Json(HealthResponse {
            status: node_status,
            accepting_transactions: accepting,
            backpressure_causes,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use std::sync::Arc;
    use tokio::sync::watch;
    use zksync_os_observability::ComponentHealthReporter;
    use zksync_os_pipeline_health::{ComponentId, PipelineHealthConfig};
    use zksync_os_types::TransactionAcceptanceState;

    fn make_state() -> AppState {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);
        let (reporter, health_rx) = ComponentHealthReporter::new("block_executor");
        reporter.record_processed(12345, None);
        AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_health: Arc::new(vec![(ComponentId::BlockExecutor, health_rx)]),
            pipeline_health_config: PipelineHealthConfig::default(),
            adjacency: Arc::new(vec![]),
        }
    }

    #[tokio::test]
    async fn healthy_node_returns_200() {
        let state = State(make_state());
        let (status, Json(body)) = health(state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.status, NodeStatus::Healthy);
        assert!(body.accepting_transactions);
        assert!(body.backpressure_causes.is_empty());
    }

    #[tokio::test]
    async fn terminating_node_returns_503() {
        let mut state = make_state();
        let (_tx2, rx2) = watch::channel(true);
        state.stop_receiver = rx2;
        let (status, Json(body)) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::Terminating);
    }

    #[tokio::test]
    async fn backpressure_returns_503_with_causes() {
        use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};
        let mut state = make_state();
        let cause = BackpressureCause {
            component: "fri_job_manager",
            trigger: BackpressureTrigger::BlockLagTooHigh {
                threshold: 500,
                actual: 782,
            },
        };
        let (_tx, rx) = watch::channel(TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure {
                causes: vec![cause],
            },
        ));
        state.acceptance_state = rx;
        let (status, Json(body)) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::Backpressure);
        assert!(!body.accepting_transactions);
        assert_eq!(body.backpressure_causes.len(), 1);
        assert_eq!(body.backpressure_causes[0].component, "fri_job_manager");
        assert_eq!(body.backpressure_causes[0].trigger, "block_lag_too_high");
        assert_eq!(body.backpressure_causes[0].threshold_blocks, Some(500));
        assert_eq!(body.backpressure_causes[0].actual_blocks, Some(782));
    }

    #[tokio::test]
    async fn time_lag_backpressure_serializes_correctly() {
        use std::time::Duration;
        use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};
        let mut state = make_state();
        let cause = BackpressureCause {
            component: "block_applier",
            trigger: BackpressureTrigger::TimeLagTooHigh {
                threshold: Duration::from_secs(30),
                actual: Duration::from_secs(45),
            },
        };
        let (_tx, rx) = watch::channel(TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure {
                causes: vec![cause],
            },
        ));
        state.acceptance_state = rx;
        let (status, Json(body)) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::Backpressure);
        assert_eq!(body.backpressure_causes[0].trigger, "time_lag_too_high");
        assert_eq!(body.backpressure_causes[0].threshold_secs, Some(30.0));
        assert_eq!(body.backpressure_causes[0].actual_secs, Some(45.0));
    }
}
