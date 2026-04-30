use crate::config::{BackpressureConfig, ComponentId, is_pipeline_stage};
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
            let up = upstream.1.block_processed.as_ref()?;
            // Fall back to picked if the downstream hasn't finished its first item yet —
            // otherwise the pair would be invisible until the first processed watermark is set.
            let down = downstream
                .1
                .block_processed
                .as_ref()
                .or(downstream.1.block_picked.as_ref())?;
            let block_diff = up.block_number.saturating_sub(down.block_number);
            let time_diff = match (up.timestamp, down.timestamp) {
                (Some(u), Some(d)) => Some(Duration::from_secs(u.saturating_sub(d))),
                _ => None,
            };
            let down_batch = downstream.1.batch_processed.or(downstream.1.batch_picked);
            let batch_diff = upstream
                .1
                .batch_processed
                .zip(down_batch)
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
    stop_receiver: watch::Receiver<bool>,
}

impl BackpressureMonitor {
    pub fn new(config: BackpressureConfig, stop_receiver: watch::Receiver<bool>) -> Self {
        let (acceptance_tx, _) = watch::channel(TransactionAcceptanceState::Accepting);
        Self {
            config,
            acceptance_tx,
            stop_receiver,
        }
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
            "BackpressureMonitor: pipeline - {}",
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
            .find_map(|(_, h)| {
                h.block_processed
                    .as_ref()
                    .map(|c| (c.block_number, c.timestamp))
            })
            .unwrap_or((0, None));

        for (index, (id, h)) in snapshot.iter().enumerate() {
            let comp_block = h
                .block_processed
                .as_ref()
                .map(|c| c.block_number)
                .unwrap_or(0);
            let comp_ts = h.block_processed.as_ref().and_then(|c| c.timestamp);
            MONITOR_METRICS.component_order[id].set(index as u64);
            MONITOR_METRICS.backpressure_active[id]
                .set(active_components.contains(id.as_str()) as u64);
            MONITOR_METRICS.component_last_processed_block[id].set(comp_block);
            MONITOR_METRICS.component_block_diff_to_head[id]
                .set(head_block.saturating_sub(comp_block));
            let time_diff_to_head: f64 = match (comp_ts, head_ts) {
                (Some(comp), Some(head)) => head.saturating_sub(comp) as f64,
                _ => 0.0,
            };
            MONITOR_METRICS.component_time_diff_to_head_seconds[id].set(time_diff_to_head);

            if let Some(bn) = h.batch_processed {
                MONITOR_METRICS.component_last_processed_batch[id].set(bn);
            }
            if let Some(bp) = h.batch_picked {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackpressureConfig, ComponentId};
    use std::time::Duration;
    use zksync_os_observability::ComponentStateReporter;
    use zksync_os_pipeline::ComponentStateReceivers;
    use zksync_os_types::BackpressureTrigger;

    fn snapshot(components: &ComponentStateReceivers) -> PipelineSnapshot {
        components
            .iter()
            .map(|(id, rx)| (*id, rx.borrow().clone()))
            .collect()
    }

    fn config_for(
        id: ComponentId,
        condition: crate::config::PipelineCondition,
    ) -> BackpressureConfig {
        let mut config = BackpressureConfig::default();
        config.set(id, condition);
        config
    }

    fn multi_config(
        ids: &[ComponentId],
        condition: crate::config::PipelineCondition,
    ) -> BackpressureConfig {
        let mut config = BackpressureConfig::default();
        for &id in ids {
            config.set(id, condition.clone());
        }
        config
    }

    #[test]
    fn above_lag_threshold_triggers() {
        let config = config_for(
            ComponentId::BlockApplier,
            crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(10),
                ..Default::default()
            },
        );
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(ComponentId::BlockApplier, 15, None, None);
        assert!(matches!(
            result.into_iter().next().map(|c| c.trigger),
            Some(BackpressureTrigger::BlockDiffToUpstreamTooHigh {
                threshold: 10,
                actual: 15
            })
        ));
    }

    #[test]
    fn at_exact_threshold_no_trigger() {
        let config = config_for(
            ComponentId::BlockApplier,
            crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(10),
                ..Default::default()
            },
        );
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(ComponentId::BlockApplier, 10, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn time_diff_to_upstream_triggers_when_exceeded() {
        let config = config_for(
            ComponentId::BlockApplier,
            crate::config::PipelineCondition {
                max_time_diff_to_upstream: Some(Duration::from_secs(30)),
                ..Default::default()
            },
        );
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(
            ComponentId::BlockApplier,
            0,
            Some(Duration::from_secs(40)),
            None,
        );
        assert!(matches!(
            result.into_iter().next().map(|c| c.trigger),
            Some(BackpressureTrigger::TimeDiffToUpstreamTooHigh { .. })
        ));
    }

    #[test]
    fn batch_component_default_threshold_triggers_above_1000() {
        // Batch-level stages default to max_batch_diff_to_upstream = 1000.
        let config = BackpressureConfig::default();
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(ComponentId::BatchVerification, 0, None, Some(1001));
        assert!(matches!(
            result.into_iter().next().map(|c| c.trigger),
            Some(BackpressureTrigger::BatchDiffToUpstreamTooHigh {
                threshold: 1000,
                actual: 1001
            })
        ));
    }

    #[test]
    fn batch_component_at_default_threshold_no_trigger() {
        let config = BackpressureConfig::default();
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(ComponentId::BatchVerification, 0, None, Some(1000));
        assert!(result.is_empty());
    }

    #[test]
    fn no_condition_set_never_triggers_for_untracked_component() {
        // Components that are neither block-level nor batch-level stages (e.g. BlockExecutor,
        // which is the head of the block pipeline) have no default threshold.
        let config = BackpressureConfig::default();
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(
            ComponentId::BlockExecutor,
            10_000,
            Some(Duration::from_secs(999_999)),
            None,
        );
        assert!(result.is_empty());
    }

    #[test]
    fn block_level_stage_default_threshold_triggers() {
        // Block-level stages get max_block_diff_to_upstream = 100 by default.
        let config = BackpressureConfig::default();
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(ComponentId::BlockApplier, 101, None, None);
        assert!(matches!(
            result.into_iter().next().map(|c| c.trigger),
            Some(BackpressureTrigger::BlockDiffToUpstreamTooHigh {
                threshold: 100,
                actual: 101
            })
        ));
    }

    #[test]
    fn block_level_stage_at_default_threshold_no_trigger() {
        let config = BackpressureConfig::default();
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(ComponentId::BlockApplier, 100, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn counter_does_not_increment_on_reason_change() {
        let cond = crate::config::PipelineCondition {
            max_block_diff_to_upstream: Some(10),
            ..Default::default()
        };
        let config = multi_config(
            &[ComponentId::BlockExecutor, ComponentId::BlockApplier],
            cond,
        );
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (reporter, rx) = ComponentStateReporter::new("block_applier");
        exec_reporter.record_processed(100, None, None);
        let components: ComponentStateReceivers = vec![
            (ComponentId::BlockExecutor, exec_rx),
            (ComponentId::BlockApplier, rx),
        ];

        reporter.record_processed(85, None, None);
        reporter.record_picked(85, None, None);
        monitor.evaluate_and_update(&snapshot(&components));
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));

        reporter.record_processed(80, None, None);
        reporter.record_picked(80, None, None);
        monitor.evaluate_and_update(&snapshot(&components));
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));

        reporter.record_processed(100, None, None);
        reporter.record_picked(100, None, None);
        monitor.evaluate_and_update(&snapshot(&components));
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting
        );
    }

    #[test]
    fn evaluate_collects_both_block_and_time_diff_to_upstream() {
        let config = config_for(
            ComponentId::BlockApplier,
            crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(10),
                max_time_diff_to_upstream: Some(Duration::from_secs(30)),
                max_batch_diff_to_upstream: None,
            },
        );
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let causes = monitor.evaluate(
            ComponentId::BlockApplier,
            15,
            Some(Duration::from_secs(40)),
            None,
        );
        assert_eq!(causes.len(), 2);
    }

    #[test]
    fn mid_pipeline_lag_does_not_cascade_to_downstream() {
        // canon is 100 blocks behind exec (>> threshold=10, triggers).
        // apply is only 2 blocks behind canon (< threshold=10, must NOT trigger).
        // Verifies that backpressure is per-adjacent-pair, not cumulative from head.
        let cond = crate::config::PipelineCondition {
            max_block_diff_to_upstream: Some(10),
            ..Default::default()
        };
        let config = multi_config(
            &[
                ComponentId::BlockExecutor,
                ComponentId::BlockCanonizer,
                ComponentId::BlockApplier,
            ],
            cond,
        );
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);

        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (canon_reporter, canon_rx) = ComponentStateReporter::new("block_canonizer");
        let (apply_reporter, apply_rx) = ComponentStateReporter::new("block_applier");
        exec_reporter.record_processed(200, None, None);
        canon_reporter.record_processed(100, None, None);
        canon_reporter.record_picked(100, None, None);
        apply_reporter.record_processed(98, None, None);
        apply_reporter.record_picked(98, None, None);

        let components: ComponentStateReceivers = vec![
            (ComponentId::BlockExecutor, exec_rx),
            (ComponentId::BlockCanonizer, canon_rx),
            (ComponentId::BlockApplier, apply_rx),
        ];
        monitor.evaluate_and_update(&snapshot(&components));

        match &*monitor.acceptance_tx.borrow() {
            TransactionAcceptanceState::NotAccepting(reasons) => {
                if let NotAcceptingReason::PipelineBackpressure { causes } = &reasons[0] {
                    assert_eq!(causes.len(), 1, "only BlockCanonizer should trigger");
                    assert_eq!(causes[0].component, "block_canonizer");
                } else {
                    panic!("expected PipelineBackpressure reason");
                }
            }
            other => panic!("expected NotAccepting, got {:?}", other),
        }
    }

    #[test]
    fn fri_job_manager_skipped_gapless_committer_adjacent_to_batch_verification() {
        // FriJobManager is excluded from the adjacency window by is_pipeline_stage
        // (hardcoded topology rule). GaplessCommitter must be measured against BatchVerification,
        // skipping FriJobManager regardless of which thresholds are configured.
        //
        // GaplessCommitter is a batch-level component; its threshold and lag are in batch space.
        // BatchVerification has no upstream in this test so it can never trigger — only GC is
        // configured.
        let mut config = BackpressureConfig::default();
        config.set(
            ComponentId::GaplessCommitter,
            crate::config::PipelineCondition {
                max_batch_diff_to_upstream: Some(3),
                ..Default::default()
            },
        );
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let (bv_reporter, bv_rx) = ComponentStateReporter::new("batch_verification");
        let (fri_reporter, fri_rx) = ComponentStateReporter::new("fri_job_manager");
        let (gc_reporter, gc_rx) = ComponentStateReporter::new("gapless_committer");

        let components: ComponentStateReceivers = vec![
            (ComponentId::BatchVerification, bv_rx),
            (ComponentId::FriJobManager, fri_rx),
            (ComponentId::GaplessCommitter, gc_rx),
        ];

        // BatchVerification at batch=10, FriJobManager at batch=5 (large lag — expected for
        // proving), GaplessCommitter at batch=6 (lag=4 vs BatchVerification, above threshold=3).
        bv_reporter.record_processed(100, None, Some(10));
        fri_reporter.record_processed(60, None, Some(5));
        fri_reporter.record_picked(60, None, Some(5));
        gc_reporter.record_processed(85, None, Some(6));
        gc_reporter.record_picked(85, None, Some(6));

        monitor.evaluate_and_update(&snapshot(&components));
        // FriJobManager lag=5 must NOT trigger (excluded from window).
        // GaplessCommitter batch lag=4 vs BatchVerification exceeds threshold=3 — must trigger.
        assert!(
            matches!(
                *monitor.acceptance_tx.borrow(),
                TransactionAcceptanceState::NotAccepting(_)
            ),
            "GaplessCommitter batch lag=4 vs BatchVerification must trigger; FriJobManager lag must not"
        );

        // GaplessCommitter catches up — batch lag drops to 2, below threshold=3, clears.
        gc_reporter.record_processed(98, None, Some(8));
        gc_reporter.record_picked(98, None, Some(8));
        monitor.evaluate_and_update(&snapshot(&components));
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting,
            "GaplessCommitter catching up must clear backpressure"
        );
    }

    #[test]
    fn l1_sender_triggers_max_batch_diff_to_upstream_via_last_processed() {
        let config = multi_config(
            &[ComponentId::UpgradeGatekeeper, ComponentId::L1SenderCommit],
            crate::config::PipelineCondition {
                max_batch_diff_to_upstream: Some(3),
                ..Default::default()
            },
        );
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (up_reporter, up_rx) = ComponentStateReporter::new("upgrade_gatekeeper");
        let (l1_reporter, l1_rx) = ComponentStateReporter::new("l1_sender_commit");
        exec_reporter.record_processed(100, None, None);
        up_reporter.record_processed(100, None, Some(10));
        l1_reporter.record_processed(60, None, Some(5));

        let components: ComponentStateReceivers = vec![
            (ComponentId::BlockExecutor, exec_rx),
            (ComponentId::UpgradeGatekeeper, up_rx),
            (ComponentId::L1SenderCommit, l1_rx),
        ];
        monitor.evaluate_and_update(&snapshot(&components));

        match &*monitor.acceptance_tx.borrow() {
            TransactionAcceptanceState::NotAccepting(reasons) => {
                let cause = reasons
                    .iter()
                    .find_map(|r| {
                        if let NotAcceptingReason::PipelineBackpressure { causes } = r {
                            causes.iter().find(|c| c.component == "l1_sender_commit")
                        } else {
                            None
                        }
                    })
                    .expect("expected l1_sender_commit cause");
                assert!(
                    matches!(
                        cause.trigger,
                        BackpressureTrigger::BatchDiffToUpstreamTooHigh {
                            threshold: 3,
                            actual: 5
                        }
                    ),
                    "expected BatchDiffToUpstreamTooHigh 10−5=5 > threshold 3, got {:?}",
                    cause.trigger
                );
            }
            other => panic!(
                "expected NotAccepting with l1_sender_commit cause, got {:?}",
                other
            ),
        }
    }
}
