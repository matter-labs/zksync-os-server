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
    pub causes: Vec<CauseJson>,
}

#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct CauseJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<&'static str>,
    pub trigger: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_blocks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_blocks: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_batches: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_batches: Option<u64>,
}

pub(crate) async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthResponse>) {
    let is_terminating = *state.stop_receiver.borrow();
    let acceptance = state.acceptance_state.borrow().clone();
    let accepting = matches!(acceptance, TransactionAcceptanceState::Accepting);

    let causes: Vec<CauseJson> = match &acceptance {
        TransactionAcceptanceState::NotAccepting(reasons) => reasons
            .iter()
            .flat_map(|reason| match reason {
                NotAcceptingReason::BlockProductionDisabled => {
                    vec![CauseJson {
                        component: None,
                        trigger: "block_production_disabled",
                        threshold_secs: None,
                        actual_secs: None,
                        threshold_blocks: None,
                        actual_blocks: None,
                        threshold_batches: None,
                        actual_batches: None,
                    }]
                }
                NotAcceptingReason::PipelineBackpressure { causes } => causes
                    .iter()
                    .map(|c| match &c.trigger {
                        BackpressureTrigger::TimeLagTooHigh { threshold, actual } => CauseJson {
                            component: Some(c.component),
                            trigger: "time_lag_too_high",
                            threshold_secs: Some(threshold.as_secs_f64()),
                            actual_secs: Some(actual.as_secs_f64()),
                            threshold_blocks: None,
                            actual_blocks: None,
                            threshold_batches: None,
                            actual_batches: None,
                        },
                        BackpressureTrigger::BlockLagTooHigh { threshold, actual } => CauseJson {
                            component: Some(c.component),
                            trigger: "block_lag_too_high",
                            threshold_secs: None,
                            actual_secs: None,
                            threshold_blocks: Some(*threshold),
                            actual_blocks: Some(*actual),
                            threshold_batches: None,
                            actual_batches: None,
                        },
                        BackpressureTrigger::BatchLagTooHigh { threshold, actual } => CauseJson {
                            component: Some(c.component),
                            trigger: "batch_lag_too_high",
                            threshold_secs: None,
                            actual_secs: None,
                            threshold_blocks: None,
                            actual_blocks: None,
                            threshold_batches: Some(*threshold),
                            actual_batches: Some(*actual),
                        },
                    })
                    .collect(),
            })
            .collect(),
        TransactionAcceptanceState::Accepting => vec![],
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
            causes,
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
        assert!(body.causes.is_empty());
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
        let (_tx, rx) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::PipelineBackpressure {
                causes: vec![cause],
            },
        ]));
        state.acceptance_state = rx;
        let (status, Json(body)) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::Backpressure);
        assert!(!body.accepting_transactions);
        assert_eq!(body.causes.len(), 1);
        assert_eq!(body.causes[0].component, Some("fri_job_manager"));
        assert_eq!(body.causes[0].trigger, "block_lag_too_high");
        assert_eq!(body.causes[0].threshold_blocks, Some(500));
        assert_eq!(body.causes[0].actual_blocks, Some(782));
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
        let (_tx, rx) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::PipelineBackpressure {
                causes: vec![cause],
            },
        ]));
        state.acceptance_state = rx;
        let (status, Json(body)) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::Backpressure);
        assert_eq!(body.causes[0].trigger, "time_lag_too_high");
        assert_eq!(body.causes[0].threshold_secs, Some(30.0));
        assert_eq!(body.causes[0].actual_secs, Some(45.0));
    }

    #[tokio::test]
    async fn batch_lag_backpressure_serializes_with_batch_fields() {
        use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};
        let mut state = make_state();
        let cause = BackpressureCause {
            component: "snark_job_manager",
            trigger: BackpressureTrigger::BatchLagTooHigh {
                threshold: 3,
                actual: 7,
            },
        };
        let (_tx, rx) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::PipelineBackpressure {
                causes: vec![cause],
            },
        ]));
        state.acceptance_state = rx;
        let (status, Json(body)) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.causes[0].trigger, "batch_lag_too_high");
        assert_eq!(body.causes[0].threshold_batches, Some(3));
        assert_eq!(body.causes[0].actual_batches, Some(7));
        // block fields must NOT be set for a batch-lag cause
        assert!(body.causes[0].threshold_blocks.is_none());
        assert!(body.causes[0].actual_blocks.is_none());
    }

    #[tokio::test]
    async fn block_production_disabled_returns_503_with_cause() {
        use zksync_os_types::NotAcceptingReason;
        let mut state = make_state();
        let (_tx, rx) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::BlockProductionDisabled,
        ]));
        state.acceptance_state = rx;
        let (status, Json(body)) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::Backpressure);
        assert!(!body.accepting_transactions);
        assert_eq!(body.causes.len(), 1);
        assert_eq!(body.causes[0].trigger, "block_production_disabled");
        assert!(body.causes[0].component.is_none());
        assert!(body.causes[0].threshold_blocks.is_none());
        assert!(body.causes[0].actual_blocks.is_none());
        assert!(body.causes[0].threshold_secs.is_none());
        assert!(body.causes[0].actual_secs.is_none());
    }

    #[tokio::test]
    async fn both_reasons_active_shows_all_causes() {
        use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};
        let mut state = make_state();
        let pipeline_cause = BackpressureCause {
            component: "batcher",
            trigger: BackpressureTrigger::BlockLagTooHigh {
                threshold: 100,
                actual: 200,
            },
        };
        let (_tx, rx) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::BlockProductionDisabled,
            NotAcceptingReason::PipelineBackpressure {
                causes: vec![pipeline_cause],
            },
        ]));
        state.acceptance_state = rx;
        let (status, Json(body)) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.causes.len(), 2);
        assert!(
            body.causes
                .iter()
                .any(|c| c.trigger == "block_production_disabled")
        );
        assert!(
            body.causes
                .iter()
                .any(|c| c.trigger == "block_lag_too_high")
        );
    }
}
