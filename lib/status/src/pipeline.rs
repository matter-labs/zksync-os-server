use crate::AppState;
use axum::{Json, extract::State};
use serde::Serialize;
use zksync_os_pipeline_health::{ComponentId, compute_adjacent_snapshots};

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
    /// Last block number received by this component. Recorded at **receive time**, before any
    /// storage writes or downstream population complete. Do not treat this as a durability
    /// guarantee — a block may appear here while it is still being persisted.
    pub last_processed_block: u64,
    /// Blocks behind the pipeline head (BlockExecutor). Always present.
    pub head_block_lag: u64,
    /// Blocks behind this component's direct upstream neighbour. This is the value compared
    /// against `max_block_lag` for backpressure. Absent for components with no registered
    /// upstream adjacency (e.g. BlockExecutor itself), which are measured head-relative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjacent_block_lag: Option<u64>,
    /// Seconds behind the pipeline head measured by block timestamps. Always present.
    pub head_time_lag_secs: f64,
    /// Seconds behind this component's direct upstream neighbour measured by block timestamps.
    /// This is the value compared against `max_time_lag_secs` for backpressure. Absent for
    /// components with no registered upstream adjacency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjacent_time_lag_secs: Option<f64>,
}

#[derive(Serialize)]
pub struct ThresholdsJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_block_lag: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_time_lag_secs: Option<f64>,
}

pub(crate) async fn pipeline(State(state): State<AppState>) -> Json<PipelineResponse> {
    let (head_block, head_ts) = state
        .component_health
        .iter()
        .find(|(id, _)| *id == ComponentId::BlockExecutor)
        .map(|(_, rx)| {
            let h = rx.borrow();
            (
                h.last_processed_block_number.unwrap_or(0),
                h.last_processed_block_timestamp,
            )
        })
        .unwrap_or((0, None));

    // Snapshot seq and timestamp for each component once.
    let component_snapshots: std::collections::HashMap<ComponentId, (u64, Option<u64>)> = state
        .component_health
        .iter()
        .map(|(id, rx)| {
            let h = rx.borrow();
            (
                *id,
                (
                    h.last_processed_block_number.unwrap_or(0),
                    h.last_processed_block_timestamp,
                ),
            )
        })
        .collect();

    // adjacent_snapshot[downstream].block_diff = upstream_seq − downstream_seq
    // adjacent_snapshot[downstream].time_diff  = upstream_ts  − downstream_ts (when both available)
    // Fan-in freedom is asserted by PipelineHealthMonitor::run() at startup; compute_adjacent_snapshots
    // will not panic here under correct wiring.
    let adjacent = compute_adjacent_snapshots(&state.adjacency, &component_snapshots);

    let now = tokio::time::Instant::now();
    let components = state
        .component_health
        .iter()
        .map(|(id, rx)| {
            let h = rx.borrow();
            let elapsed = now.duration_since(h.state_entered_at).as_secs_f64();
            let head_block_lag =
                head_block.saturating_sub(h.last_processed_block_number.unwrap_or(0));
            let head_time_lag_secs = match (h.last_processed_block_timestamp, head_ts) {
                (Some(comp_ts), Some(h_ts)) => h_ts.saturating_sub(comp_ts) as f64,
                _ => 0.0,
            };
            let cond = state.pipeline_health_config.condition_for(*id);
            ComponentEntryWithThresholds {
                name: id.as_str(),
                snapshot: ComponentSnapshot {
                    state: h.state.as_str(),
                    state_duration_secs: elapsed,
                    last_processed_block: h.last_processed_block_number.unwrap_or(0),
                    head_block_lag,
                    adjacent_block_lag: adjacent.get(id).map(|s| s.block_diff),
                    head_time_lag_secs,
                    adjacent_time_lag_secs: adjacent
                        .get(id)
                        .and_then(|s| s.time_diff)
                        .map(|d| d.as_secs_f64()),
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
            adjacency: Arc::new(vec![]),
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

    /// upstream_diff must reflect the adjacent diff (upstream_seq − downstream_seq), not the
    /// head-relative lag, so that operators can compare it directly against max_block_lag.
    #[tokio::test]
    async fn upstream_diff_reflects_adjacent_lag_not_head_lag() {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);

        // head (BlockExecutor) at block 100
        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        exec_reporter.record_processed(100, None);

        // downstream (BlockApplier) at block 90 — head-lag = 10, adjacent diff = 5
        let (applier_reporter, applier_rx) = ComponentHealthReporter::new("block_applier");
        applier_reporter.record_processed(90, None);

        // intermediate (BlockCanonizer) at block 95
        let (canonizer_reporter, canonizer_rx) = ComponentHealthReporter::new("block_canonizer");
        canonizer_reporter.record_processed(95, None);

        let state = AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_health: Arc::new(vec![
                (ComponentId::BlockExecutor, exec_rx),
                (ComponentId::BlockCanonizer, canonizer_rx),
                (ComponentId::BlockApplier, applier_rx),
            ]),
            pipeline_health_config: PipelineHealthConfig::default(),
            // Canonizer is upstream of Applier; diff = 95 − 90 = 5
            adjacency: Arc::new(vec![
                (ComponentId::BlockExecutor, ComponentId::BlockCanonizer),
                (ComponentId::BlockCanonizer, ComponentId::BlockApplier),
            ]),
        };

        let Json(body) = pipeline(State(state)).await;

        let exec = body
            .components
            .iter()
            .find(|c| c.name == "block_executor")
            .unwrap();
        let canonizer = body
            .components
            .iter()
            .find(|c| c.name == "block_canonizer")
            .unwrap();
        let applier = body
            .components
            .iter()
            .find(|c| c.name == "block_applier")
            .unwrap();

        // BlockExecutor has no upstream adjacency — adjacent_block_lag must be absent
        assert!(exec.snapshot.adjacent_block_lag.is_none());
        // BlockCanonizer upstream is BlockExecutor (100); diff = 100 − 95 = 5
        assert_eq!(canonizer.snapshot.adjacent_block_lag, Some(5));
        // BlockApplier upstream is BlockCanonizer (95); diff = 95 − 90 = 5
        assert_eq!(applier.snapshot.adjacent_block_lag, Some(5));
        // head-relative lag is still present and differs from adjacent lag
        assert_eq!(applier.snapshot.head_block_lag, 10);
    }
}
