use crate::config::{BackpressureConfig, ComponentId, is_pipeline_stage};
use crate::gate::{PipelineAdmissionGate, PipelineAdmissionReceiver};
use crate::metrics::MONITOR_METRICS;
use reth_tasks::Runtime;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::watch;
use zksync_os_observability::ComponentState;
use zksync_os_types::{
    BackpressureCause, BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState,
};

/// Ordered list of pipeline component states (pipeline order).
pub type PipelineSnapshot = Vec<(ComponentId, ComponentState)>;

/// Lag between two adjacent pipeline stages: how far the downstream component is behind its upstream neighbor.
pub struct AdjacentSnapshot {
    /// Number of blocks the downstream stage is behind the upstream stage.
    pub block_diff: u64,
    /// Diff between the last processed block timestamps of the two stages.
    pub time_diff: Option<Duration>,
    /// Number of batches the downstream stage is behind the upstream stage.
    pub batch_diff: Option<u64>,
}

fn compute_adjacent_snapshots(
    snapshot: &PipelineSnapshot,
) -> HashMap<ComponentId, AdjacentSnapshot> {
    snapshot
        .iter()
        .filter(|(id, _)| is_pipeline_stage(*id))
        .collect::<Vec<_>>()
        .windows(2)
        .filter_map(|w| {
            let (upstream, downstream) = (&w[0], &w[1]);
            let up = upstream.1.processed.as_ref()?;
            let down = downstream.1.processed.as_ref()?;
            let block_diff = up.block_number.saturating_sub(down.block_number);
            let time_diff = match (up.timestamp, down.timestamp) {
                (Some(u), Some(d)) => Some(Duration::from_secs(u.saturating_sub(d))),
                _ => None,
            };
            let batch_diff = up
                .batch_number
                .zip(down.batch_number)
                .map(|(u, d)| u.saturating_sub(d));
            Some((
                downstream.0,
                AdjacentSnapshot {
                    block_diff,
                    time_diff,
                    batch_diff,
                },
            ))
        })
        .collect()
}

pub struct BackpressureMonitor {
    config: BackpressureConfig,
    acceptance_tx: watch::Sender<TransactionAcceptanceState>,
    pipeline_gate: PipelineAdmissionGate,
    stop_receiver: watch::Receiver<bool>,
}

impl BackpressureMonitor {
    pub fn new(config: BackpressureConfig, stop_receiver: watch::Receiver<bool>) -> Self {
        let (acceptance_tx, _) = watch::channel(TransactionAcceptanceState::Accepting);
        let pipeline_gate = PipelineAdmissionGate::new();
        Self {
            config,
            acceptance_tx,
            pipeline_gate,
            stop_receiver,
        }
    }

    pub fn subscribe_gate(&self) -> PipelineAdmissionReceiver {
        self.pipeline_gate.subscribe()
    }

    pub fn spawn(
        self,
        runtime: &Runtime,
        snapshot_rx: watch::Receiver<PipelineSnapshot>,
    ) -> watch::Receiver<TransactionAcceptanceState> {
        let acceptance_rx = self.acceptance_tx.subscribe();
        runtime.spawn_critical_task("backpressure monitor", self.run(snapshot_rx));
        acceptance_rx
    }

    pub async fn run(mut self, mut snapshot_rx: watch::Receiver<PipelineSnapshot>) {
        MONITOR_METRICS.accepting.set(1);

        let snapshot = snapshot_rx.borrow_and_update().clone();
        self.log_startup_summary(&snapshot);

        // Guard against a race where stop is already set before run() is entered.
        if *self.stop_receiver.borrow_and_update() {
            return;
        }

        // Snapshot current state immediately so operators see accurate lag at monitor startup.
        self.evaluate_and_update(&snapshot);

        loop {
            tokio::select! {
                result = snapshot_rx.changed() => {
                    match result {
                        Ok(()) => {
                            self.evaluate_and_update(&snapshot_rx.borrow_and_update());
                        }
                        Err(_) => return,
                    }
                }
                _ = self.stop_receiver.changed() => {
                    tracing::info!("BackpressureMonitor: stop signal received");
                    return;
                }
            }
        }
    }

    fn log_startup_summary(&self, snapshot: &PipelineSnapshot) {
        let mut chain: Vec<String> = Vec::new();
        for (id, _) in snapshot {
            if !is_pipeline_stage(*id) {
                continue;
            }
            let cond = self.config.condition_for(*id);
            let mut thresholds: Vec<String> = Vec::new();
            if let Some(v) = cond.max_block_diff_to_upstream {
                thresholds.push(format!("block≤{v}"));
                MONITOR_METRICS.backpressure_threshold_block_diff_to_upstream[id].set(v);
            }
            if let Some(v) = cond.max_time_diff_to_upstream {
                thresholds.push(format!("time≤{}s", v.as_secs()));
                MONITOR_METRICS.backpressure_threshold_time_diff_to_upstream_seconds[id]
                    .set(v.as_secs_f64());
            }
            if let Some(v) = cond.max_batch_diff_to_upstream {
                thresholds.push(format!("batch≤{v}"));
                MONITOR_METRICS.backpressure_threshold_batch_diff_to_upstream[id].set(v);
            }
            if thresholds.is_empty() {
                chain.push(id.as_str().to_string());
            } else {
                chain.push(format!("{} ({})", id.as_str(), thresholds.join(", ")));
            }
        }
        tracing::info!(
            "Pipeline order: {}",
            if chain.is_empty() {
                "none".to_string()
            } else {
                chain.join(" → ")
            },
        );
    }

    fn evaluate_and_update(&self, snapshot: &PipelineSnapshot) {
        let adjacent = compute_adjacent_snapshots(snapshot);
        let new_state = self.compute_acceptance_state(snapshot, &adjacent);
        self.emit_metrics(snapshot, &adjacent, &new_state);
        self.update_acceptance_state(new_state);
    }

    fn compute_acceptance_state(
        &self,
        snapshot: &PipelineSnapshot,
        adjacent: &HashMap<ComponentId, AdjacentSnapshot>,
    ) -> TransactionAcceptanceState {
        let mut active_causes: Vec<BackpressureCause> = Vec::new();

        for (id, _) in snapshot {
            let adj = adjacent.get(id);
            let block_diff = adj.map(|s| s.block_diff).unwrap_or(0);
            let time_diff = adj.and_then(|s| s.time_diff);
            let batch_diff = adj.and_then(|s| s.batch_diff);
            active_causes.extend(self.evaluate(*id, block_diff, time_diff, batch_diff));
        }

        active_causes.sort_by_key(|c| c.component);

        if active_causes.is_empty() {
            TransactionAcceptanceState::Accepting
        } else {
            TransactionAcceptanceState::NotAccepting(vec![
                NotAcceptingReason::PipelineBackpressure {
                    causes: active_causes,
                },
            ])
        }
    }

    fn update_acceptance_state(&self, new_state: TransactionAcceptanceState) {
        self.pipeline_gate
            .set(matches!(new_state, TransactionAcceptanceState::Accepting));
        self.acceptance_tx.send_if_modified(|current| {
            if *current == new_state {
                return false;
            }
            match (&*current, &new_state) {
                (
                    TransactionAcceptanceState::Accepting,
                    TransactionAcceptanceState::NotAccepting(reasons),
                ) => {
                    tracing::warn!(
                        "pipeline backpressure: suspending transaction acceptance. Reasons: {reasons:?}"
                    );
                    MONITOR_METRICS.acceptance_state_changes.inc();
                    MONITOR_METRICS.accepting.set(0);
                }
                (
                    TransactionAcceptanceState::NotAccepting(_),
                    TransactionAcceptanceState::Accepting,
                ) => {
                    tracing::info!(
                        "pipeline backpressure cleared: resuming transaction acceptance"
                    );
                    MONITOR_METRICS.acceptance_state_clears.inc();
                    MONITOR_METRICS.accepting.set(1);
                }
                (
                    TransactionAcceptanceState::NotAccepting(_),
                    TransactionAcceptanceState::NotAccepting(reasons),
                ) => {
                    tracing::debug!(
                        "pipeline backpressure cause set changed while already suspended. Reasons: {reasons:?}"
                    );
                }
                _ => {}
            }
            *current = new_state.clone();
            true
        });
    }

    fn emit_metrics(
        &self,
        snapshot: &PipelineSnapshot,
        adjacent: &HashMap<ComponentId, AdjacentSnapshot>,
        state: &TransactionAcceptanceState,
    ) {
        let active_components: HashSet<&str> = match state {
            TransactionAcceptanceState::NotAccepting(reasons) => reasons
                .iter()
                .flat_map(|r| match r {
                    NotAcceptingReason::PipelineBackpressure { causes } => causes.as_slice(),
                    _ => &[],
                })
                .map(|c| c.component)
                .collect(),
            TransactionAcceptanceState::Accepting => HashSet::new(),
        };

        let (head_block, head_ts) = snapshot
            .iter()
            .find_map(|(_, h)| h.processed.as_ref().map(|c| (c.block_number, c.timestamp)))
            .unwrap_or((0, None));

        for (index, (id, h)) in snapshot.iter().enumerate() {
            let comp_block = h.processed.as_ref().map(|c| c.block_number).unwrap_or(0);
            let comp_ts = h.processed.as_ref().and_then(|c| c.timestamp);
            MONITOR_METRICS.component_order[id].set(index as u64);
            MONITOR_METRICS.backpressure_active[id]
                .set(active_components.contains(id.as_str()) as u64);
            let picked_block = h.picked.as_ref().map(|c| c.block_number).unwrap_or(0);
            MONITOR_METRICS.component_last_picked_block[id].set(picked_block);
            MONITOR_METRICS.component_last_processed_block[id].set(comp_block);
            MONITOR_METRICS.component_block_diff_to_head[id]
                .set(head_block.saturating_sub(comp_block));
            let time_diff_to_head: f64 = match (comp_ts, head_ts) {
                (Some(comp), Some(head)) => head.saturating_sub(comp) as f64,
                _ => 0.0,
            };
            MONITOR_METRICS.component_time_diff_to_head_seconds[id].set(time_diff_to_head);

            if let Some(bn) = h.processed.as_ref().and_then(|c| c.batch_number) {
                MONITOR_METRICS.component_last_processed_batch[id].set(bn);
            }
            if let Some(bp) = h.picked.as_ref().and_then(|c| c.batch_number) {
                MONITOR_METRICS.component_last_picked_batch[id].set(bp);
            }
        }

        for (&id, snap) in adjacent {
            MONITOR_METRICS.component_block_diff_to_upstream[&id].set(snap.block_diff);
            let time_diff_secs = snap.time_diff.map(|d| d.as_secs_f64()).unwrap_or(0.0);
            MONITOR_METRICS.component_time_diff_to_upstream_seconds[&id].set(time_diff_secs);
            if let Some(batch_diff) = snap.batch_diff {
                MONITOR_METRICS.component_batch_diff_to_upstream[&id].set(batch_diff);
            }
        }
    }

    fn evaluate(
        &self,
        id: ComponentId,
        block_diff_to_upstream: u64,
        time_diff_to_upstream: Option<Duration>,
        batch_diff_to_upstream: Option<u64>,
    ) -> Vec<BackpressureCause> {
        let condition = self.config.condition_for(id);
        let mut causes = Vec::new();

        if let Some(max_diff) = condition.max_block_diff_to_upstream
            && block_diff_to_upstream > max_diff
        {
            causes.push(BackpressureCause {
                component: id.as_str(),
                trigger: BackpressureTrigger::BlockDiffToUpstreamTooHigh {
                    threshold: max_diff,
                    actual: block_diff_to_upstream,
                },
            });
        }

        if let (Some(max_time_diff_to_upstream), Some(actual)) =
            (condition.max_time_diff_to_upstream, time_diff_to_upstream)
            && actual > max_time_diff_to_upstream
        {
            causes.push(BackpressureCause {
                component: id.as_str(),
                trigger: BackpressureTrigger::TimeDiffToUpstreamTooHigh {
                    threshold: max_time_diff_to_upstream,
                    actual,
                },
            });
        }

        if let (Some(max_batch), Some(actual)) =
            (condition.max_batch_diff_to_upstream, batch_diff_to_upstream)
            && actual > max_batch
        {
            causes.push(BackpressureCause {
                component: id.as_str(),
                trigger: BackpressureTrigger::BatchDiffToUpstreamTooHigh {
                    threshold: max_batch,
                    actual,
                },
            });
        }

        causes
    }
}
