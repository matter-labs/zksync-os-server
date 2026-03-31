use crate::AppState;
use axum::{Json, extract::State};
use serde::Serialize;
use zksync_os_pipeline_health::ComponentId;

#[derive(Serialize)]
pub struct PipelineResponse {
    pub head_block: u64,
    pub components: Vec<ComponentEntryWithThresholds>,
}

#[derive(Serialize)]
pub struct ComponentEntryWithThresholds {
    pub name: &'static str,
    #[serde(flatten)]
    pub snapshot: ComponentSnapshot,
    pub thresholds: ThresholdsJson,
}

#[derive(Serialize)]
pub struct ComponentSnapshot {
    pub state: &'static str,
    pub state_duration_secs: f64,
    pub last_processed_block: u64,
    pub block_lag: u64,
    pub time_lag_secs: f64,
}

#[derive(Serialize)]
pub struct ThresholdsJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_block_lag: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_time_lag_secs: Option<f64>,
}

pub(crate) async fn pipeline(State(state): State<AppState>) -> Json<PipelineResponse> {
    let head_block = state
        .component_health
        .iter()
        .find(|(id, _)| *id == ComponentId::BlockExecutor)
        .map(|(_, rx)| rx.borrow().last_processed_block_number.unwrap_or(0))
        .unwrap_or(0);

    let head_ts = state
        .component_health
        .iter()
        .find(|(id, _)| *id == ComponentId::BlockExecutor)
        .and_then(|(_, rx)| rx.borrow().last_processed_block_timestamp);

    let now = tokio::time::Instant::now();
    let components = state
        .component_health
        .iter()
        .map(|(id, rx)| {
            let h = rx.borrow();
            let elapsed = now.duration_since(h.state_entered_at).as_secs_f64();
            let lag = head_block.saturating_sub(h.last_processed_block_number.unwrap_or(0));
            let time_lag_secs = match (h.last_processed_block_timestamp, head_ts) {
                (Some(comp_ts), Some(h_ts)) if h_ts > comp_ts => (h_ts - comp_ts) as f64,
                _ => 0.0,
            };
            let cond = state.pipeline_health_config.condition_for(*id);
            ComponentEntryWithThresholds {
                name: id.as_str(),
                snapshot: ComponentSnapshot {
                    state: h.state.as_str(),
                    state_duration_secs: elapsed,
                    last_processed_block: h.last_processed_block_number.unwrap_or(0),
                    block_lag: lag,
                    time_lag_secs,
                },
                thresholds: ThresholdsJson {
                    max_block_lag: cond.max_block_lag,
                    max_time_lag_secs: cond.max_time_lag.map(|d| d.as_secs_f64()),
                },
            }
        })
        .collect();

    Json(PipelineResponse {
        head_block,
        components,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use std::sync::Arc;
    use tokio::sync::watch;
    use zksync_os_observability::ComponentHealthReporter;
    use zksync_os_pipeline_health::{BlockPipelineCondition, ComponentId, PipelineHealthConfig};
    use zksync_os_types::TransactionAcceptanceState;

    fn make_state(config: PipelineHealthConfig) -> AppState {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);
        let (reporter, health_rx) = ComponentHealthReporter::new("block_executor");
        reporter.record_processed(12345, None);
        AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_health: Arc::new(vec![(ComponentId::BlockExecutor, health_rx)]),
            pipeline_health_config: config,
        }
    }

    #[tokio::test]
    async fn pipeline_returns_head_block() {
        let Json(body) = pipeline(State(make_state(PipelineHealthConfig::default()))).await;
        assert_eq!(body.head_block, 12345);
    }

    #[tokio::test]
    async fn pipeline_returns_one_component_entry() {
        let Json(body) = pipeline(State(make_state(PipelineHealthConfig::default()))).await;
        assert_eq!(body.components.len(), 1);
        assert_eq!(body.components[0].name, "block_executor");
    }

    #[tokio::test]
    async fn pipeline_thresholds_reflect_config() {
        let config = PipelineHealthConfig {
            block_pipeline: BlockPipelineCondition {
                max_block_lag: Some(50),
                max_time_lag: Some(std::time::Duration::from_secs(30)),
            },
            ..PipelineHealthConfig::default()
        };
        let Json(body) = pipeline(State(make_state(config))).await;
        let t = &body.components[0].thresholds;
        assert_eq!(t.max_block_lag, Some(50));
        assert_eq!(t.max_time_lag_secs, Some(30.0));
    }

    #[tokio::test]
    async fn pipeline_thresholds_are_none_when_not_configured() {
        let Json(body) = pipeline(State(make_state(PipelineHealthConfig::default()))).await;
        let t = &body.components[0].thresholds;
        assert!(t.max_block_lag.is_none());
        assert!(t.max_time_lag_secs.is_none());
    }
}
