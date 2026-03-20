use crate::AppState;
use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;
use zksync_os_observability::GenericComponentState;
use zksync_os_pipeline_health::ComponentId;
use zksync_os_types::{BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState};

#[derive(Serialize)]
pub struct HealthResponse {
    pub healthy: bool,
    pub accepting_transactions: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub backpressure_causes: Vec<BackpressureCauseJson>,
    pub pipeline: PipelineSnapshot,
}

#[derive(Serialize)]
pub struct PipelineSnapshot {
    pub head_block: u64,
    pub components: Vec<ComponentEntry>,
}

#[derive(Serialize)]
pub struct ComponentEntry {
    pub name: &'static str,
    #[serde(flatten)]
    pub snapshot: ComponentSnapshot,
}

#[derive(Serialize)]
pub struct ComponentSnapshot {
    pub state: &'static str,
    pub state_duration_secs: f64,
    pub last_processed_block: u64,
    pub block_lag: u64,
    pub waiting_send_secs: f64,
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

    let head_block = state
        .component_health
        .iter()
        .find(|(id, _)| *id == ComponentId::BlockExecutor)
        .map(|(_, rx)| rx.borrow().last_processed_seq)
        .unwrap_or(0);

    let now = tokio::time::Instant::now();
    let components: Vec<ComponentEntry> = state
        .component_health
        .iter()
        .map(|(id, rx)| {
            let h = rx.borrow();
            let elapsed = now.duration_since(h.state_entered_at).as_secs_f64();
            let lag = head_block.saturating_sub(h.last_processed_seq);
            let waiting_send_secs = if h.state == GenericComponentState::WaitingSend {
                elapsed
            } else {
                0.0
            };
            let block_lag = if id.is_reactive() || h.state == GenericComponentState::WaitingSend {
                lag
            } else {
                0
            };
            ComponentEntry {
                name: id.as_str(),
                snapshot: ComponentSnapshot {
                    state: h.state.as_str(),
                    state_duration_secs: elapsed,
                    last_processed_block: h.last_processed_seq,
                    block_lag,
                    waiting_send_secs,
                },
            }
        })
        .collect();

    let backpressure_causes = match &acceptance {
        TransactionAcceptanceState::NotAccepting(NotAcceptingReason::PipelineBackpressure {
            causes,
        }) => causes
            .iter()
            .map(|c| match &c.trigger {
                BackpressureTrigger::WaitingSendTooLong { threshold, actual } => {
                    BackpressureCauseJson {
                        component: c.component,
                        trigger: "waiting_send_too_long",
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

    let healthy = !is_terminating && accepting;
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(HealthResponse {
            healthy,
            accepting_transactions: accepting,
            backpressure_causes,
            pipeline: PipelineSnapshot {
                head_block,
                components,
            },
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
    use zksync_os_pipeline_health::ComponentId;
    use zksync_os_types::TransactionAcceptanceState;

    fn make_state() -> AppState {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);
        let (reporter, health_rx) = ComponentHealthReporter::new("block_executor");
        reporter.record_processed(12345);
        AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_health: Arc::new(vec![(ComponentId::BlockExecutor, health_rx)]),
        }
    }

    #[tokio::test]
    async fn healthy_node_returns_200() {
        let state = State(make_state());
        let (status, Json(body)) = health(state).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.healthy);
        assert!(body.accepting_transactions);
        assert!(body.backpressure_causes.is_empty());
        assert_eq!(body.pipeline.head_block, 12345);
    }

    #[tokio::test]
    async fn terminating_node_returns_503() {
        let mut state = make_state();
        let (_tx2, rx2) = watch::channel(true);
        state.stop_receiver = rx2;
        let (status, Json(body)) = health(State(state)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!body.healthy);
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
        assert!(!body.accepting_transactions);
        assert_eq!(body.backpressure_causes.len(), 1);
        assert_eq!(body.backpressure_causes[0].component, "fri_job_manager");
        assert_eq!(body.backpressure_causes[0].trigger, "block_lag_too_high");
        assert_eq!(body.backpressure_causes[0].threshold_blocks, Some(500));
        assert_eq!(body.backpressure_causes[0].actual_blocks, Some(782));
    }
}
