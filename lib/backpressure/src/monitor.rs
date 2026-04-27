use crate::config::{BackpressureConfig, ComponentId, is_pipeline_stage};
use crate::metrics::MONITOR_METRICS;
use reth_tasks::Runtime;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::watch;
use zksync_os_observability::{ComponentState, GENERAL_METRICS};
use zksync_os_types::{
    BackpressureCause, BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState,
};

/// Ordered list of pipeline component states (pipeline order).
pub type PipelineSnapshot = Vec<(ComponentId, ComponentState)>;

pub struct AdjacentSnapshot {
    pub block_diff: u64,
    pub time_diff: Option<Duration>,
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
            let down = w[1].0;
            let up_processed = w[0].1.block_processed.as_ref()?;
            let down_processed = w[1].1.block_processed.as_ref()?;
            let block_diff = up_processed
                .block_number
                .saturating_sub(down_processed.block_number);
            let time_diff = match (up_processed.timestamp, down_processed.timestamp) {
                (Some(u), Some(d)) => Some(Duration::from_secs(u.saturating_sub(d))),
                _ => None,
            };
            let batch_diff = w[0]
                .1
                .batch_processed
                .zip(w[1].1.batch_processed)
                .map(|(u, d)| u.saturating_sub(d));
            Some((
                down,
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

        // Log startup summary.
        let mut with_threshold = Vec::new();
        let mut no_threshold = Vec::new();
        for (id, _) in &snapshot {
            if !is_pipeline_stage(*id) {
                continue;
            }
            let cond = self.config.condition_for(*id);
            let has_threshold = cond.max_block_diff_to_upstream.is_some()
                || cond.max_time_diff_to_upstream.is_some()
                || cond.max_batch_diff_to_upstream.is_some();
            if has_threshold {
                with_threshold.push(id.as_str());
            } else {
                no_threshold.push(id.as_str());
            }
            tracing::debug!(
                "BackpressureMonitor: component {} threshold — max_block_diff_to_upstream={:?}, max_time_diff_to_upstream={:?}, max_batch_diff_to_upstream={:?}",
                id.as_str(),
                cond.max_block_diff_to_upstream,
                cond.max_time_diff_to_upstream,
                cond.max_batch_diff_to_upstream,
            );
            if let Some(v) = cond.max_block_diff_to_upstream {
                MONITOR_METRICS.backpressure_threshold_block_diff_to_upstream[id].set(v);
            }
            if let Some(v) = cond.max_time_diff_to_upstream {
                MONITOR_METRICS.backpressure_threshold_time_diff_to_upstream_seconds[id]
                    .set(v.as_secs_f64());
            }
            if let Some(v) = cond.max_batch_diff_to_upstream {
                MONITOR_METRICS.backpressure_threshold_batch_diff_to_upstream[id].set(v);
            }
        }

        // Guard against a race where stop is already set before run() is entered.
        if *self.stop_receiver.borrow_and_update() {
            return;
        }

        // Snapshot current state immediately so operators see accurate lag at monitor startup.
        self.evaluate_and_update(&snapshot);

        const STATE_AGE_INTERVAL: Duration = Duration::from_secs(5);
        loop {
            tokio::select! {
                result = tokio::time::timeout(STATE_AGE_INTERVAL, snapshot_rx.changed()) => {
                    match result {
                        Ok(Ok(())) => {
                            let snapshot = snapshot_rx.borrow_and_update().clone();
                            self.evaluate_and_update(&snapshot);
                        }
                        Err(_) => {
                            self.emit_state_age(&snapshot_rx.borrow());
                        }
                        Ok(Err(_)) => return,
                    }
                }
                _ = self.stop_receiver.changed() => {
                    tracing::info!("BackpressureMonitor: stop signal received");
                    return;
                }
            }
        }
    }

    fn evaluate_and_update(&self, snapshot: &PipelineSnapshot) {
        let (head_block, head_ts) = snapshot
            .first()
            .map(|(_, h)| {
                (
                    h.block_processed
                        .as_ref()
                        .map(|c| c.block_number)
                        .unwrap_or(0),
                    h.block_processed.as_ref().and_then(|c| c.timestamp),
                )
            })
            .unwrap_or((0, None));

        // In-flight ranges — reported by components that hold multiple items concurrently.
        let mut in_flight_snapshot: HashMap<ComponentId, (u64, u64)> = HashMap::new();
        for (id, h) in snapshot {
            if let (Some(first), Some(last)) = (&h.in_flight_first_batch, &h.in_flight_last_batch) {
                in_flight_snapshot.insert(*id, (first.batch_number, last.batch_number));
            }
        }

        let adjacent = compute_adjacent_snapshots(snapshot);

        let mut active_component_ids: std::collections::HashSet<ComponentId> =
            std::collections::HashSet::new();
        let mut active_causes: Vec<BackpressureCause> = snapshot
            .iter()
            .flat_map(|(id, _)| {
                let adj = adjacent.get(id);
                let block_diff_to_upstream = adj.map(|s| s.block_diff).unwrap_or(0);
                let time_diff_to_upstream = adj.and_then(|s| s.time_diff);
                let batch_diff_to_upstream = adj.and_then(|s| s.batch_diff);
                let causes = self.evaluate(
                    *id,
                    block_diff_to_upstream,
                    time_diff_to_upstream,
                    batch_diff_to_upstream,
                );
                if !causes.is_empty() {
                    active_component_ids.insert(*id);
                }
                causes
            })
            .collect();

        active_causes.sort_by_key(|c| c.component);

        let new_state = if active_causes.is_empty() {
            TransactionAcceptanceState::Accepting
        } else {
            TransactionAcceptanceState::NotAccepting(vec![
                NotAcceptingReason::PipelineBackpressure {
                    causes: active_causes,
                },
            ])
        };

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

        // Emit metrics from the same snapshot used for the acceptance decision above.
        for (index, (id, h)) in snapshot.iter().enumerate() {
            let comp_block = h
                .block_processed
                .as_ref()
                .map(|c| c.block_number)
                .unwrap_or(0);
            let comp_ts = h.block_processed.as_ref().and_then(|c| c.timestamp);
            MONITOR_METRICS.component_order[id].set(index as u64);
            MONITOR_METRICS.backpressure_active[id].set(active_component_ids.contains(id) as u64);
            MONITOR_METRICS.component_last_processed_block[id].set(comp_block);
            MONITOR_METRICS.component_block_diff_to_head[id]
                .set(head_block.saturating_sub(comp_block));
            let time_diff_to_head: f64 = match (comp_ts, head_ts) {
                (Some(comp_ts), Some(h_ts)) => h_ts.saturating_sub(comp_ts) as f64,
                _ => 0.0,
            };
            MONITOR_METRICS.component_time_diff_to_head_seconds[id].set(time_diff_to_head);
        }

        for (&id, snap) in &adjacent {
            MONITOR_METRICS.component_block_diff_to_upstream[&id].set(snap.block_diff);
            let time_diff_secs = snap.time_diff.map(|d| d.as_secs_f64()).unwrap_or(0.0);
            MONITOR_METRICS.component_time_diff_to_upstream_seconds[&id].set(time_diff_secs);
            if let Some(batch_diff) = snap.batch_diff {
                MONITOR_METRICS.component_batch_diff_to_upstream[&id].set(batch_diff);
            }
        }

        for (id, h) in snapshot {
            if let Some(bn) = h.batch_processed {
                MONITOR_METRICS.component_last_processed_batch[id].set(bn);
            }
            if let Some(bp) = h.batch_picked {
                MONITOR_METRICS.component_last_picked_batch[id].set(bp);
            }
        }

        for (id, _) in snapshot {
            if !matches!(
                id,
                ComponentId::FriJobManager
                    | ComponentId::SnarkJobManager
                    | ComponentId::L1SenderCommit
                    | ComponentId::L1SenderProve
                    | ComponentId::L1SenderExecute
            ) {
                continue;
            }
            let (first, last, count) =
                if let Some(&(first_bn, last_bn)) = in_flight_snapshot.get(&id) {
                    (first_bn, last_bn, last_bn.saturating_sub(first_bn) + 1)
                } else {
                    (0, 0, 0)
                };
            MONITOR_METRICS.in_flight_first_batch[&id].set(first);
            MONITOR_METRICS.in_flight_last_batch[&id].set(last);
            MONITOR_METRICS.in_flight_batch_count[&id].set(count);
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

    // TODO: decide about this state age as it's not directly related to backpressure monitor
    fn emit_state_age(&self, snapshot: &PipelineSnapshot) {
        let now = tokio::time::Instant::now();
        for (id, h) in snapshot {
            let age = now
                .saturating_duration_since(h.state_entered_at)
                .as_secs_f64();
            GENERAL_METRICS.component_state_age_seconds[&(id.as_str(), h.state, h.specific_state)]
                .set(age);
        }
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
    fn below_lag_threshold_no_trigger() {
        let config = config_for(
            ComponentId::BlockApplier,
            crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(10),
                ..Default::default()
            },
        );
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(ComponentId::BlockApplier, 5, None, None);
        assert!(result.is_empty());
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
    fn no_condition_set_never_triggers() {
        let config = BackpressureConfig::default();
        let monitor = BackpressureMonitor::new(config, watch::channel(false).1);
        let result = monitor.evaluate(
            ComponentId::BlockApplier,
            10_000,
            Some(Duration::from_secs(999_999)),
            None,
        );
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn counter_does_not_increment_on_reason_change() {
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

    #[tokio::test]
    async fn fri_job_manager_skipped_gapless_committer_adjacent_to_batch_verification() {
        // FriJobManager is excluded from the adjacency window by is_pipeline_stage
        // (hardcoded topology rule). GaplessCommitter must be measured against BatchVerification,
        // skipping FriJobManager regardless of which thresholds are configured.
        let config = multi_config(
            &[
                ComponentId::BatchVerification,
                ComponentId::GaplessCommitter,
            ],
            crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(10),
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

        // BatchVerification at 100, FriJobManager at 60 (large lag — expected for proving),
        // GaplessCommitter at 85 (lag=15 vs BatchVerification, above threshold=10).
        bv_reporter.record_processed(100, None, None);
        fri_reporter.record_processed(60, None, None);
        fri_reporter.record_picked(60, None, None);
        gc_reporter.record_processed(85, None, None);
        gc_reporter.record_picked(85, None, None);

        monitor.evaluate_and_update(&snapshot(&components));
        // FriJobManager lag=40 must NOT trigger (excluded from window).
        // GaplessCommitter lag=15 vs BatchVerification exceeds threshold=10 — must trigger.
        assert!(
            matches!(
                *monitor.acceptance_tx.borrow(),
                TransactionAcceptanceState::NotAccepting(_)
            ),
            "GaplessCommitter lag=15 vs BatchVerification must trigger; FriJobManager lag must not"
        );

        // GaplessCommitter catches up — lag drops below threshold, backpressure clears.
        gc_reporter.record_processed(98, None, None);
        gc_reporter.record_picked(98, None, None);
        monitor.evaluate_and_update(&snapshot(&components));
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting,
            "GaplessCommitter catching up must clear backpressure"
        );
    }

    #[tokio::test]
    async fn l1_sender_triggers_max_batch_diff_to_upstream_via_last_processed() {
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
