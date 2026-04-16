use crate::AppState;
use axum::{Json, extract::State};
use serde::Serialize;
use zksync_os_pipeline_health::{ComponentId, PipelineMaps, compute_adjacent_snapshots};

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
    /// Block number last dequeued from the input channel (before any processing).
    /// High-watermark; absent until first item received.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_picked_block: Option<u64>,
    /// Timestamp of the last picked block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_picked_timestamp: Option<u64>,
    /// Block number last fully handled/forwarded downstream.
    /// High-watermark; absent until first item processed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_processed_block: Option<u64>,
    /// Timestamp of the last processed block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_processed_timestamp: Option<u64>,
    /// Oldest batch currently in-flight (range-processing components only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_first: Option<InFlightBatchJson>,
    /// Newest batch currently in-flight (range-processing components only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_last: Option<InFlightBatchJson>,
    /// Last completed batch number (batch-pipeline components only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_number: Option<u64>,
    /// Blocks behind the pipeline head (BlockExecutor). Always present.
    pub head_block_lag: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjacent_block_lag: Option<u64>,
    pub head_time_lag_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjacent_time_lag_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjacent_batch_lag: Option<u64>,
}

#[derive(Serialize)]
pub struct InFlightBatchJson {
    pub batch_number: u64,
    pub last_block_number: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

#[derive(Serialize)]
pub struct ThresholdsJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_block_lag: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_time_lag_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_batch_lag: Option<u64>,
}

pub(crate) async fn pipeline(State(state): State<AppState>) -> Json<PipelineResponse> {
    let (head_block, head_ts) = state
        .component_health
        .iter()
        .find(|(id, _)| *id == ComponentId::BlockExecutor)
        .map(|(_, rx)| {
            let h = rx.borrow();
            (
                h.last_processed
                    .as_ref()
                    .map(|c| c.block_number)
                    .unwrap_or(0),
                h.last_processed.as_ref().and_then(|c| c.timestamp),
            )
        })
        .unwrap_or((0, None));

    // Snapshot coordinates via PipelineMaps (shared with the health monitor) so fallback
    // policy is defined once and both code paths always agree.
    let maps = PipelineMaps::snapshot(&state.component_health);

    // adjacent_snapshot[downstream].block_diff = upstream.last_processed − downstream.last_picked
    // adjacent_snapshot[downstream].time_diff  = upstream_ts − downstream_ts (when both available)
    // Fan-in freedom is asserted by PipelineHealthMonitor::run() at startup; compute_adjacent_snapshots
    // will not panic here under correct wiring.
    let adjacent = compute_adjacent_snapshots(
        &state.adjacency,
        &maps.processed,
        &maps.picked,
        &maps.batch_processed,
        &maps.batch_picked,
    );

    let now = tokio::time::Instant::now();
    let components = state
        .component_health
        .iter()
        .map(|(id, rx)| {
            let h = rx.borrow();
            let elapsed = now.duration_since(h.state_entered_at).as_secs_f64();
            let comp_processed_seq = h
                .last_processed
                .as_ref()
                .map(|c| c.block_number)
                .unwrap_or(0);
            let comp_processed_ts = h.last_processed.as_ref().and_then(|c| c.timestamp);
            let head_block_lag = head_block.saturating_sub(comp_processed_seq);
            let head_time_lag_secs = match (comp_processed_ts, head_ts) {
                (Some(comp_ts), Some(h_ts)) => h_ts.saturating_sub(comp_ts) as f64,
                _ => 0.0,
            };
            let cond = state.pipeline_health_config.condition_for(*id);
            ComponentEntryWithThresholds {
                name: id.as_str(),
                snapshot: ComponentSnapshot {
                    state: h.state.as_str(),
                    state_duration_secs: elapsed,
                    last_picked_block: h.last_picked.as_ref().map(|c| c.block_number),
                    last_picked_timestamp: h.last_picked.as_ref().and_then(|c| c.timestamp),
                    last_processed_block: h.last_processed.as_ref().map(|c| c.block_number),
                    last_processed_timestamp: h.last_processed.as_ref().and_then(|c| c.timestamp),
                    in_flight_first: h.in_flight_first.as_ref().map(|c| InFlightBatchJson {
                        batch_number: c.batch_number,
                        last_block_number: c.last_block_number,
                        timestamp: c.timestamp,
                    }),
                    in_flight_last: h.in_flight_last.as_ref().map(|c| InFlightBatchJson {
                        batch_number: c.batch_number,
                        last_block_number: c.last_block_number,
                        timestamp: c.timestamp,
                    }),
                    batch_number: h.batch_number,
                    head_block_lag,
                    adjacent_block_lag: adjacent.get(id).map(|s| s.block_diff),
                    head_time_lag_secs,
                    adjacent_time_lag_secs: adjacent
                        .get(id)
                        .and_then(|s| s.time_diff)
                        .map(|d| d.as_secs_f64()),
                    adjacent_batch_lag: adjacent.get(id).and_then(|s| s.batch_diff),
                },
                thresholds: ThresholdsJson {
                    max_block_lag: cond.max_block_lag,
                    max_time_lag_secs: cond.max_time_lag.map(|d| d.as_secs_f64()),
                    max_batch_lag: cond.max_batch_lag,
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

    /// upstream_diff must reflect the adjacent diff
    /// (upstream.last_processed − downstream.last_picked), not the head-relative lag, so that
    /// operators can compare it directly against max_block_lag.
    #[tokio::test]
    async fn upstream_diff_reflects_adjacent_lag_not_head_lag() {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);

        // head (BlockExecutor) at block 100
        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        exec_reporter.record_processed(100, None);

        // BlockCanonizer: processed 95, picked 95 → adjacent diff from Executor = 100 − 95 = 5
        let (canonizer_reporter, canonizer_rx) = ComponentHealthReporter::new("block_canonizer");
        canonizer_reporter.record_processed(95, None);
        canonizer_reporter.record_picked(95, None);

        // BlockApplier: processed 90, picked 90 → adjacent diff from Canonizer = 95 − 90 = 5
        // head-lag = 100 − 90 = 10 (head-relative, different from adjacent)
        let (applier_reporter, applier_rx) = ComponentHealthReporter::new("block_applier");
        applier_reporter.record_processed(90, None);
        applier_reporter.record_picked(90, None);

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

    #[tokio::test]
    async fn adjacent_lag_zero_when_channel_drained() {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);

        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        exec_reporter.record_processed(100, None);
        exec_reporter.record_picked(100, None);

        let (applier_reporter, applier_rx) = ComponentHealthReporter::new("block_applier");
        applier_reporter.record_picked(100, None); // picked everything the executor produced
        applier_reporter.record_processed(80, None); // but only finished 80 so far

        let state = AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_health: Arc::new(vec![
                (ComponentId::BlockExecutor, exec_rx),
                (ComponentId::BlockApplier, applier_rx),
            ]),
            pipeline_health_config: PipelineHealthConfig::default(),
            adjacency: Arc::new(vec![(
                ComponentId::BlockExecutor,
                ComponentId::BlockApplier,
            )]),
        };

        let Json(body) = pipeline(State(state)).await;
        let applier = body
            .components
            .iter()
            .find(|c| c.name == "block_applier")
            .unwrap();
        // channel is empty (upstream processed == downstream picked) → lag = 0
        assert_eq!(applier.snapshot.adjacent_block_lag, Some(0));
        // head lag still reflects that applier is 20 blocks behind on processing
        assert_eq!(applier.snapshot.head_block_lag, 20);
    }
}
