use crate::AppState;
use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;
use zksync_os_types::{BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState};

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Accepting,
    NotAccepting,
    Terminating,
}

#[derive(Serialize)]
pub struct AcceptingResponse {
    pub status: NodeStatus,
    pub accepting_transactions: bool,
    pub causes: Vec<CauseJson>,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "trigger", rename_all = "snake_case")]
pub(crate) enum CauseJson {
    BlockProductionDisabled,
    TimeDiffToUpstreamTooHigh {
        component: &'static str,
        threshold_secs: f64,
        actual_secs: f64,
    },
    BlockDiffToUpstreamTooHigh {
        component: &'static str,
        threshold_blocks: u64,
        actual_blocks: u64,
    },
    BatchDiffToUpstreamTooHigh {
        component: &'static str,
        threshold_batches: u64,
        actual_batches: u64,
    },
}

pub(crate) async fn accepting(
    State(state): State<AppState>,
) -> (StatusCode, Json<AcceptingResponse>) {
    let is_terminating = *state.stop_receiver.borrow();
    let acceptance = state.acceptance_state.borrow().clone();
    let accepting = matches!(acceptance, TransactionAcceptanceState::Accepting);

    let causes: Vec<CauseJson> = match &acceptance {
        TransactionAcceptanceState::NotAccepting(reasons) => reasons
            .iter()
            .flat_map(|reason| match reason {
                NotAcceptingReason::BlockProductionDisabled => {
                    vec![CauseJson::BlockProductionDisabled]
                }
                NotAcceptingReason::PipelineBackpressure { causes } => causes
                    .iter()
                    .map(|c| match &c.trigger {
                        BackpressureTrigger::TimeDiffToUpstreamTooHigh { threshold, actual } => {
                            CauseJson::TimeDiffToUpstreamTooHigh {
                                component: c.component,
                                threshold_secs: threshold.as_secs_f64(),
                                actual_secs: actual.as_secs_f64(),
                            }
                        }
                        BackpressureTrigger::BlockDiffToUpstreamTooHigh { threshold, actual } => {
                            CauseJson::BlockDiffToUpstreamTooHigh {
                                component: c.component,
                                threshold_blocks: *threshold,
                                actual_blocks: *actual,
                            }
                        }
                        BackpressureTrigger::BatchDiffToUpstreamTooHigh { threshold, actual } => {
                            CauseJson::BatchDiffToUpstreamTooHigh {
                                component: c.component,
                                threshold_batches: *threshold,
                                actual_batches: *actual,
                            }
                        }
                    })
                    .collect(),
            })
            .collect(),
        TransactionAcceptanceState::Accepting => vec![],
    };

    let node_status = if is_terminating {
        NodeStatus::Terminating
    } else if accepting {
        NodeStatus::Accepting
    } else {
        NodeStatus::NotAccepting
    };

    let http_status = if matches!(node_status, NodeStatus::Accepting) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        http_status,
        Json(AcceptingResponse {
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
    use zksync_os_backpressure::{BackpressureConfig, ComponentId};
    use zksync_os_observability::ComponentStateReporter;
    use zksync_os_types::TransactionAcceptanceState;

    fn make_state() -> AppState {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);
        let (reporter, state_rx) = ComponentStateReporter::new("block_executor");
        reporter.record_processed(12345, None, None);
        AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_states: Arc::new(vec![(ComponentId::BlockExecutor, state_rx)]),
            edges: Arc::new(vec![]),
            backpressure_config: BackpressureConfig::default(),
        }
    }

    #[tokio::test]
    async fn accepting_node_returns_200() {
        let state = State(make_state());
        let (status, Json(body)) = accepting(state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.status, NodeStatus::Accepting);
        assert!(body.accepting_transactions);
        assert!(body.causes.is_empty());
    }

    #[tokio::test]
    async fn terminating_node_returns_503() {
        let mut state = make_state();
        let (_tx2, rx2) = watch::channel(true);
        state.stop_receiver = rx2;
        let (status, Json(body)) = accepting(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::Terminating);
    }

    #[tokio::test]
    async fn backpressure_returns_503_with_causes() {
        use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};
        let mut state = make_state();
        let cause = BackpressureCause {
            component: "fri_job_manager",
            trigger: BackpressureTrigger::BlockDiffToUpstreamTooHigh {
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
        let (status, Json(body)) = accepting(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::NotAccepting);
        assert!(!body.accepting_transactions);
        assert_eq!(body.causes.len(), 1);
        assert_eq!(
            body.causes[0],
            CauseJson::BlockDiffToUpstreamTooHigh {
                component: "fri_job_manager",
                threshold_blocks: 500,
                actual_blocks: 782,
            }
        );
    }

    #[tokio::test]
    async fn time_diff_to_upstream_backpressure_serializes_correctly() {
        use std::time::Duration;
        use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};
        let mut state = make_state();
        let cause = BackpressureCause {
            component: "block_applier",
            trigger: BackpressureTrigger::TimeDiffToUpstreamTooHigh {
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
        let (status, Json(body)) = accepting(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::NotAccepting);
        assert_eq!(
            body.causes[0],
            CauseJson::TimeDiffToUpstreamTooHigh {
                component: "block_applier",
                threshold_secs: 30.0,
                actual_secs: 45.0,
            }
        );
        let wire = serde_json::to_value(&body).expect("serialize AcceptingResponse");
        assert_eq!(
            wire["causes"][0]["trigger"].as_str(),
            Some("time_diff_to_upstream_too_high"),
            "wire trigger string must be 'time_diff_to_upstream_too_high'; got: {wire}"
        );
    }

    #[tokio::test]
    async fn batch_diff_to_upstream_backpressure_serializes_with_batch_fields() {
        use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};
        let mut state = make_state();
        let cause = BackpressureCause {
            component: "snark_job_manager",
            trigger: BackpressureTrigger::BatchDiffToUpstreamTooHigh {
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
        let (status, Json(body)) = accepting(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body.causes[0],
            CauseJson::BatchDiffToUpstreamTooHigh {
                component: "snark_job_manager",
                threshold_batches: 3,
                actual_batches: 7,
            }
        );
        let wire = serde_json::to_value(&body).expect("serialize AcceptingResponse");
        assert_eq!(
            wire["causes"][0]["trigger"].as_str(),
            Some("batch_diff_to_upstream_too_high"),
            "wire trigger string must be 'batch_diff_to_upstream_too_high'; got: {wire}"
        );
    }

    #[tokio::test]
    async fn block_production_disabled_returns_503_with_cause() {
        use zksync_os_types::NotAcceptingReason;
        let mut state = make_state();
        let (_tx, rx) = watch::channel(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::BlockProductionDisabled,
        ]));
        state.acceptance_state = rx;
        let (status, Json(body)) = accepting(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, NodeStatus::NotAccepting);
        assert!(!body.accepting_transactions);
        assert_eq!(body.causes.len(), 1);
        assert_eq!(body.causes[0], CauseJson::BlockProductionDisabled);
    }

    #[tokio::test]
    async fn both_reasons_active_shows_all_causes() {
        use zksync_os_types::{BackpressureCause, BackpressureTrigger, NotAcceptingReason};
        let mut state = make_state();
        let pipeline_cause = BackpressureCause {
            component: "batcher",
            trigger: BackpressureTrigger::BlockDiffToUpstreamTooHigh {
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
        let (status, Json(body)) = accepting(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.causes.len(), 2);
        assert!(
            body.causes
                .iter()
                .any(|c| matches!(c, CauseJson::BlockProductionDisabled))
        );
        assert!(
            body.causes
                .iter()
                .any(|c| matches!(c, CauseJson::BlockDiffToUpstreamTooHigh { .. }))
        );
    }
}
