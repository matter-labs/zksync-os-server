use crate::AppState;
use axum::{Json, extract::State};
use serde::Serialize;
use zksync_os_backpressure::{ComponentId, PipelineMaps, compute_adjacent_snapshots};
use zksync_os_observability::ComponentState;

#[derive(Serialize)]
pub struct PipelineResponse {
    /// Head block number as reported by BlockExecutor. `None` when BlockExecutor
    /// is not registered or has not yet processed any block — distinct from block 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_block: Option<u64>,
    pub components: Vec<ComponentEntryWithThresholds>,
}

#[derive(Serialize)]
pub struct ComponentEntryWithThresholds {
    pub name: &'static str,
    /// Pipeline position rank. Stable ordering contract for JSON consumers that reshape
    /// or re-sort the `components` array. Sourced from `ComponentId::pipeline_order()`.
    pub order: u64,
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
    /// Oldest batch currently in-flight. Populated by prover managers and L1 senders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_first: Option<InFlightBatchJson>,
    /// Newest batch currently in-flight. Populated by prover managers and L1 senders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_last: Option<InFlightBatchJson>,
    /// Last completed batch number (batch-pipeline components only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_number: Option<u64>,
    /// Blocks behind the pipeline head (BlockExecutor). Absent until BlockExecutor
    /// has processed its first block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_diff_to_head: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_diff_to_upstream: Option<u64>,
    pub time_diff_to_head_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_diff_to_upstream_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_diff_to_upstream: Option<u64>,
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
    pub max_block_diff_to_upstream: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_time_diff_to_upstream_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_batch_diff_to_upstream: Option<u64>,
}

pub(crate) async fn pipeline(State(state): State<AppState>) -> Json<PipelineResponse> {
    // Single observation per component so head_block, adjacent diffs, and per-row
    // fields in the response all reflect the same point in time. Re-borrowing mid-
    // handler would allow the JSON to show arithmetic that does not close.
    let snaps: Vec<(ComponentId, ComponentState)> = state
        .component_states
        .iter()
        .map(|(id, rx)| (*id, rx.borrow().clone()))
        .collect();

    let (head_block, head_ts) = snaps
        .iter()
        .find(|(id, _)| *id == ComponentId::BlockExecutor)
        .and_then(|(_, h)| {
            h.last_processed
                .as_ref()
                .map(|c| (c.block_number, c.timestamp))
        })
        .map_or((None, None), |(bn, ts)| (Some(bn), ts));

    // Shared with the state monitor so fallback policy is defined once.
    let maps = PipelineMaps::snapshot_from(&snaps);

    // adjacent_snapshot[downstream].block_diff = upstream.last_processed − downstream.last_picked
    // adjacent_snapshot[downstream].time_diff  = upstream_ts − downstream_ts (when both available)
    // Edges come from the monitor's declared topology (not registration order), so
    // components registered outside the linear pipe chain — e.g. EN's PriorityTree,
    // whose declared upstream is BlockApplier but whose registration-order neighbour
    // is BatchVerificationResponder — resolve to the same upstream here as they do
    // in the monitor's Prometheus / acceptance path.
    let adjacent = compute_adjacent_snapshots(
        &state.edges,
        &maps.processed,
        &maps.picked,
        &maps.batch_processed,
        &maps.batch_picked,
    );

    let now = tokio::time::Instant::now();
    let components = snaps
        .iter()
        .map(|(id, h)| {
            let elapsed = now.duration_since(h.state_entered_at).as_secs_f64();
            let comp_processed_block = h
                .last_processed
                .as_ref()
                .map(|c| c.block_number)
                .unwrap_or(0);
            let comp_processed_ts = h.last_processed.as_ref().and_then(|c| c.timestamp);
            let block_diff_to_head = head_block.map(|h| h.saturating_sub(comp_processed_block));
            let time_diff_to_head_secs = match (comp_processed_ts, head_ts) {
                (Some(comp_ts), Some(h_ts)) => h_ts.saturating_sub(comp_ts) as f64,
                _ => 0.0,
            };
            let cond = state.backpressure_config.condition_for(*id);
            ComponentEntryWithThresholds {
                name: id.as_str(),
                order: id.pipeline_order(),
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
                    block_diff_to_head,
                    block_diff_to_upstream: adjacent.get(id).map(|s| s.block_diff),
                    time_diff_to_head_secs,
                    time_diff_to_upstream_secs: adjacent
                        .get(id)
                        .and_then(|s| s.time_diff)
                        .map(|d| d.as_secs_f64()),
                    batch_diff_to_upstream: adjacent.get(id).and_then(|s| s.batch_diff),
                },
                thresholds: ThresholdsJson {
                    max_block_diff_to_upstream: cond.max_block_diff_to_upstream,
                    max_time_diff_to_upstream_secs: cond
                        .max_time_diff_to_upstream
                        .map(|d| d.as_secs_f64()),
                    max_batch_diff_to_upstream: cond.max_batch_diff_to_upstream,
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
    use zksync_os_backpressure::{BackpressureConfig, ComponentId, PipelineCondition};
    use zksync_os_observability::ComponentStateReporter;
    use zksync_os_types::TransactionAcceptanceState;

    fn make_state(config: BackpressureConfig) -> AppState {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);
        let (reporter, state_rx) = ComponentStateReporter::new("block_executor");
        reporter.record_processed(12345, None, None);
        AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_states: Arc::new(vec![(ComponentId::BlockExecutor, state_rx)]),
            edges: Arc::new(vec![]),
            backpressure_config: config,
        }
    }

    #[tokio::test]
    async fn pipeline_returns_head_block() {
        let Json(body) = pipeline(State(make_state(BackpressureConfig::default()))).await;
        assert_eq!(body.head_block, Some(12345));
    }

    #[tokio::test]
    async fn pipeline_returns_one_component_entry() {
        let Json(body) = pipeline(State(make_state(BackpressureConfig::default()))).await;
        assert_eq!(body.components.len(), 1);
        assert_eq!(body.components[0].name, "block_executor");
    }

    #[tokio::test]
    async fn pipeline_thresholds_reflect_config() {
        let config = BackpressureConfig {
            block_pipeline: PipelineCondition {
                max_block_diff_to_upstream: Some(50),
                max_time_diff_to_upstream: Some(std::time::Duration::from_secs(30)),
                max_batch_diff_to_upstream: None,
            },
            ..BackpressureConfig::default()
        };
        let Json(body) = pipeline(State(make_state(config))).await;
        let t = &body.components[0].thresholds;
        assert_eq!(t.max_block_diff_to_upstream, Some(50));
        assert_eq!(t.max_time_diff_to_upstream_secs, Some(30.0));
    }

    #[tokio::test]
    async fn pipeline_thresholds_are_none_when_not_configured() {
        let Json(body) = pipeline(State(make_state(BackpressureConfig::default()))).await;
        let t = &body.components[0].thresholds;
        assert!(t.max_block_diff_to_upstream.is_none());
        assert!(t.max_time_diff_to_upstream_secs.is_none());
    }

    /// block_diff_to_upstream must reflect the adjacent diff
    /// (upstream.last_processed − downstream.last_picked), not the head-relative diff, so that
    /// operators can compare it directly against max_block_diff_to_upstream.
    #[tokio::test]
    async fn block_diff_to_upstream_reflects_adjacent_not_head() {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);

        // head (BlockExecutor) at block 100
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        exec_reporter.record_processed(100, None, None);

        // BlockCanonizer: processed 95, picked 95 → adjacent diff from Executor = 100 − 95 = 5
        let (canonizer_reporter, canonizer_rx) = ComponentStateReporter::new("block_canonizer");
        canonizer_reporter.record_processed(95, None, None);
        canonizer_reporter.record_picked(95, None, None);

        // BlockApplier: processed 90, picked 90 → adjacent diff from Canonizer = 95 − 90 = 5
        // head-lag = 100 − 90 = 10 (head-relative, different from adjacent)
        let (applier_reporter, applier_rx) = ComponentStateReporter::new("block_applier");
        applier_reporter.record_processed(90, None, None);
        applier_reporter.record_picked(90, None, None);

        let state = AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_states: Arc::new(vec![
                (ComponentId::BlockExecutor, exec_rx),
                (ComponentId::BlockCanonizer, canonizer_rx),
                (ComponentId::BlockApplier, applier_rx),
            ]),
            edges: Arc::new(vec![
                (ComponentId::BlockExecutor, ComponentId::BlockCanonizer),
                (ComponentId::BlockCanonizer, ComponentId::BlockApplier),
            ]),
            backpressure_config: BackpressureConfig::default(),
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

        // BlockExecutor has no upstream adjacency — block_diff_to_upstream must be absent
        assert!(exec.snapshot.block_diff_to_upstream.is_none());
        // BlockCanonizer upstream is BlockExecutor (100); diff = 100 − 95 = 5
        assert_eq!(canonizer.snapshot.block_diff_to_upstream, Some(5));
        // BlockApplier upstream is BlockCanonizer (95); diff = 95 − 90 = 5
        assert_eq!(applier.snapshot.block_diff_to_upstream, Some(5));
        // head-relative diff is still present and differs from adjacent diff
        assert_eq!(applier.snapshot.block_diff_to_head, Some(10));
    }

    /// /status/pipeline must compute adjacency from the declared edges (the same ones
    /// the BackpressureMonitor uses for Prometheus and the acceptance decision), not
    /// from component registration order. The EN PriorityTree case is the canonical
    /// divergence: PriorityTree is registered *after* BatchVerificationResponder for
    /// unrelated invariants, but its declared upstream is BlockApplier. Deriving edges
    /// from order would silently report a different `block_diff_to_upstream` than Prometheus.
    #[tokio::test]
    async fn block_diff_to_upstream_uses_declared_edges_not_registration_order() {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);

        // Head.
        let (exec_r, exec_rx) = ComponentStateReporter::new("block_executor");
        exec_r.record_processed(100, None, None);

        // PriorityTree's declared upstream.
        let (applier_r, applier_rx) = ComponentStateReporter::new("block_applier");
        applier_r.record_processed(95, None, None);
        applier_r.record_picked(95, None, None);

        // PriorityTree's registration-order neighbour (NOT its declared upstream).
        // Give it a distinct block number so the wrong upstream would yield a distinct
        // (and wrong) block_diff_to_upstream.
        let (verif_r, verif_rx) = ComponentStateReporter::new("batch_verification_responder");
        verif_r.record_processed(90, None, None);
        verif_r.record_picked(90, None, None);

        let (pt_r, pt_rx) = ComponentStateReporter::new("priority_tree");
        pt_r.record_picked(80, None, None);

        // Explicit declared edges — mirror what MonitorHandle would record for EN:
        // the linear pipe chain up to BatchVerificationResponder, plus a side edge
        // BlockApplier → PriorityTree.
        let edges = Arc::new(vec![
            (ComponentId::BlockExecutor, ComponentId::BlockApplier),
            (
                ComponentId::BlockApplier,
                ComponentId::BatchVerificationResponder,
            ),
            (ComponentId::BlockApplier, ComponentId::PriorityTree),
        ]);

        let state = AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_states: Arc::new(vec![
                (ComponentId::BlockExecutor, exec_rx),
                (ComponentId::BlockApplier, applier_rx),
                (ComponentId::BatchVerificationResponder, verif_rx),
                (ComponentId::PriorityTree, pt_rx),
            ]),
            edges,
            backpressure_config: BackpressureConfig::default(),
        };

        let Json(body) = pipeline(State(state)).await;
        let pt = body
            .components
            .iter()
            .find(|c| c.name == "priority_tree")
            .unwrap();

        // Declared edge: BlockApplier.processed (95) − PriorityTree.picked (80) = 15.
        // Registration-order derivation would give BatchVerificationResponder (90) − 80 = 10.
        assert_eq!(pt.snapshot.block_diff_to_upstream, Some(15));
    }

    #[tokio::test]
    async fn block_diff_to_upstream_zero_when_channel_drained() {
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (_accept_tx, accept_rx) = watch::channel(TransactionAcceptanceState::Accepting);

        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        exec_reporter.record_processed(100, None, None);
        exec_reporter.record_picked(100, None, None);

        let (applier_reporter, applier_rx) = ComponentStateReporter::new("block_applier");
        applier_reporter.record_picked(100, None, None); // picked everything the executor produced
        applier_reporter.record_processed(80, None, None); // but only finished 80 so far

        let state = AppState {
            stop_receiver: stop_rx,
            acceptance_state: accept_rx,
            component_states: Arc::new(vec![
                (ComponentId::BlockExecutor, exec_rx),
                (ComponentId::BlockApplier, applier_rx),
            ]),
            edges: Arc::new(vec![(
                ComponentId::BlockExecutor,
                ComponentId::BlockApplier,
            )]),
            backpressure_config: BackpressureConfig::default(),
        };

        let Json(body) = pipeline(State(state)).await;
        let applier = body
            .components
            .iter()
            .find(|c| c.name == "block_applier")
            .unwrap();
        // channel is empty (upstream processed == downstream picked) → diff = 0
        assert_eq!(applier.snapshot.block_diff_to_upstream, Some(0));
        // head diff still reflects that applier is 20 blocks behind on processing
        assert_eq!(applier.snapshot.block_diff_to_head, Some(20));
    }
}
