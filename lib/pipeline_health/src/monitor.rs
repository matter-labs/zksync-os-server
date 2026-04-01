use crate::config::{ComponentId, PipelineHealthConfig};
use crate::metrics::MONITOR_METRICS;
use futures::stream::{StreamExt, select_all};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::WatchStream;
use zksync_os_observability::ComponentHealth;
use zksync_os_types::{
    BackpressureCause, BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState,
};

pub struct PipelineHealthMonitor {
    config: PipelineHealthConfig,
    components: Vec<(ComponentId, watch::Receiver<ComponentHealth>)>,
    adjacency: Vec<(ComponentId, ComponentId)>,
    acceptance_tx: watch::Sender<TransactionAcceptanceState>,
    stop_receiver: watch::Receiver<bool>,
}

impl PipelineHealthMonitor {
    pub fn new(
        config: PipelineHealthConfig,
        stop_receiver: watch::Receiver<bool>,
    ) -> (Self, watch::Receiver<TransactionAcceptanceState>) {
        assert!(
            config.metrics_interval > Duration::ZERO,
            "PipelineHealthConfig::metrics_interval must be > 0"
        );
        let (acceptance_tx, acceptance_rx) = watch::channel(TransactionAcceptanceState::Accepting);
        (
            Self {
                config,
                components: vec![],
                adjacency: vec![],
                acceptance_tx,
                stop_receiver,
            },
            acceptance_rx,
        )
    }

    pub fn register(&mut self, id: ComponentId, receiver: watch::Receiver<ComponentHealth>) {
        self.components.push((id, receiver));
    }

    pub fn register_adjacency(&mut self, upstream: ComponentId, downstream: ComponentId) {
        self.adjacency.push((upstream, downstream));
    }

    /// Returns the set of component IDs that have been registered via [`register`].
    /// Useful for filtering adjacency pairs before calling [`register_adjacency`].
    pub fn registered_component_ids(&self) -> std::collections::HashSet<ComponentId> {
        self.components.iter().map(|(id, _)| *id).collect()
    }

    pub(crate) fn compute_adjacent_diffs(&self) -> std::collections::HashMap<ComponentId, u64> {
        let seqs: std::collections::HashMap<ComponentId, u64> = self
            .components
            .iter()
            .map(|(id, rx)| (*id, rx.borrow().last_processed_block_number.unwrap_or(0)))
            .collect();

        self.adjacency
            .iter()
            .map(|&(up, down)| {
                let up_seq = *seqs.get(&up).unwrap_or_else(|| {
                    panic!(
                        "adjacency upstream component {:?} is not registered; \
                         call register() for every component before register_adjacency()",
                        up
                    )
                });
                let down_seq = *seqs.get(&down).unwrap_or_else(|| {
                    panic!(
                        "adjacency downstream component {:?} is not registered; \
                         call register() for every component before register_adjacency()",
                        down
                    )
                });
                (down, up_seq.saturating_sub(down_seq))
            })
            .collect()
    }

    pub async fn run(mut self) {
        // Initial state is Accepting; set the gauge so it is correct before any transition fires.
        MONITOR_METRICS.accepting.set(1);

        // Prometheus metrics timer — independent of health evaluation.
        let mut metrics_tick = tokio::time::interval(self.config.metrics_interval);
        metrics_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        assert!(
            !self.components.is_empty(),
            "PipelineHealthMonitor::run called with no registered components; \
             call register() before run()"
        );

        // Every component referenced in an adjacency pair must be registered.
        // Unregistered components would silently produce zero seq numbers, causing
        // huge spurious diffs and false backpressure.
        {
            let registered: std::collections::HashSet<ComponentId> =
                self.components.iter().map(|(id, _)| *id).collect();
            for &(up, down) in &self.adjacency {
                assert!(
                    registered.contains(&up),
                    "adjacency upstream component {:?} is not registered; \
                     call register() for every component before register_adjacency()",
                    up
                );
                assert!(
                    registered.contains(&down),
                    "adjacency downstream component {:?} is not registered; \
                     call register() for every component before register_adjacency()",
                    down
                );
            }
        }

        // Build a merged stream of all component health changes.
        // WatchStream::from_changes skips the initial value and only yields on subsequent
        // changes — this is intentional. Components that are stuck at their initial state
        // (e.g. never processed a block) are detected on the next periodic metrics_tick
        // (default 5 s) rather than immediately. The trade-off is accepted: the periodic
        // tick is the safety net for components that do not produce change events.
        let streams = self
            .components
            .iter()
            .map(|(_, rx)| WatchStream::from_changes(rx.clone()))
            .collect::<Vec<_>>();
        let mut combined = select_all(streams);

        loop {
            tokio::select! {
                Some(_) = combined.next() => self.evaluate_and_update(),
                _ = metrics_tick.tick() => self.evaluate_and_update(),
                _ = self.stop_receiver.changed() => {
                    tracing::info!("PipelineHealthMonitor: stop signal received");
                    return;
                }
            }
        }
    }

    fn head_state(&self) -> (u64, Option<u64>) {
        match self
            .components
            .iter()
            .find(|(id, _)| *id == ComponentId::BlockExecutor)
        {
            Some((_, rx)) => {
                let h = rx.borrow();
                (
                    h.last_processed_block_number.unwrap_or(0),
                    h.last_processed_block_timestamp,
                )
            }
            None => {
                tracing::warn!(
                    "PipelineHealthMonitor: BlockExecutor is not registered; \
                     block_lag and time_lag metrics will be unreliable (all components \
                     will show head-relative lag from block 0)"
                );
                (0, None)
            }
        }
    }

    fn evaluate_and_update(&self) {
        let (head_seq, head_ts) = self.head_state();
        self.evaluate_and_update_with_head(head_seq, head_ts);
    }

    pub(crate) fn evaluate_and_update_with_head(&self, head_seq: u64, head_ts: Option<u64>) {
        // Pre-compute adjacent diffs for backpressure evaluation. Using adjacent diff
        // (upstream_seq − component_seq) instead of head-relative lag prevents cascade
        // false-positives: a mid-pipeline bottleneck should not cause all downstream
        // components to appear as independent backpressure causes.
        // Note: the Prometheus `component_block_lag` metric still uses head-relative lag
        // for observability (operators can see absolute distance from pipeline head).
        // The adjacent diff view is exposed separately via `component_block_diff_to_upstream`.
        let adjacent_diffs = self.compute_adjacent_diffs();

        let mut active_component_ids: std::collections::HashSet<ComponentId> =
            std::collections::HashSet::new();
        let mut active_causes: Vec<BackpressureCause> = self
            .components
            .iter()
            .flat_map(|(id, rx)| {
                let health = rx.borrow();
                let block_lag = adjacent_diffs.get(id).copied().unwrap_or_else(|| {
                    head_seq.saturating_sub(health.last_processed_block_number.unwrap_or(0))
                });
                let causes = self.evaluate(*id, &health, block_lag, head_ts);
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
            TransactionAcceptanceState::NotAccepting(NotAcceptingReason::PipelineBackpressure {
                causes: active_causes,
            })
        };

        self.acceptance_tx.send_if_modified(|current| {
            if *current == new_state {
                return false;
            }
            match (&*current, &new_state) {
                (
                    TransactionAcceptanceState::Accepting,
                    TransactionAcceptanceState::NotAccepting(reason),
                ) => {
                    tracing::warn!(
                        ?reason,
                        "pipeline backpressure: suspending transaction acceptance"
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
                // Cause set changed while already NotAccepting — log at debug for visibility.
                (
                    TransactionAcceptanceState::NotAccepting(_),
                    TransactionAcceptanceState::NotAccepting(reason),
                ) => {
                    tracing::debug!(
                        ?reason,
                        "pipeline backpressure cause set changed while already suspended"
                    );
                }
                _ => {}
            }
            *current = new_state.clone();
            true
        });

        // Emit metrics always in sync with current evaluation result.
        for (id, rx) in &self.components {
            let health = rx.borrow();
            MONITOR_METRICS.backpressure_active[id].set(active_component_ids.contains(id) as u64);
            MONITOR_METRICS.component_last_processed_block[id]
                .set(health.last_processed_block_number.unwrap_or(0));
            MONITOR_METRICS.component_block_lag[id]
                .set(head_seq.saturating_sub(health.last_processed_block_number.unwrap_or(0)));
            let time_lag = match (health.last_processed_block_timestamp, head_ts) {
                (Some(comp_ts), Some(h_ts)) => h_ts.saturating_sub(comp_ts) as f64,
                _ => 0.0,
            };
            MONITOR_METRICS.component_time_lag_seconds[id].set(time_lag);
        }

        for (&id, &diff) in &adjacent_diffs {
            MONITOR_METRICS.component_block_diff_to_upstream[&id].set(diff);
        }
    }

    pub(crate) fn evaluate(
        &self,
        id: ComponentId,
        health: &ComponentHealth,
        block_lag: u64,
        head_ts: Option<u64>,
    ) -> Vec<BackpressureCause> {
        let condition = self.config.condition_for(id);
        let mut causes = Vec::new();

        if let Some(max_lag) = condition.max_block_lag {
            // block_lag is pre-computed by caller: adjacent diff (upstream_seq − component_seq)
            // if an adjacency is registered, otherwise head_seq − component_seq.
            if block_lag > max_lag {
                causes.push(BackpressureCause {
                    component: id.as_str(),
                    trigger: BackpressureTrigger::BlockLagTooHigh {
                        threshold: max_lag,
                        actual: block_lag,
                    },
                });
            }
        }

        if let Some(max_time_lag) = condition.max_time_lag {
            // Only evaluate if both timestamps are available (Some).
            if let (Some(comp_ts), Some(h_ts)) = (health.last_processed_block_timestamp, head_ts) {
                let lag_secs = h_ts.saturating_sub(comp_ts);
                let actual = Duration::from_secs(lag_secs);
                if actual > max_time_lag {
                    causes.push(BackpressureCause {
                        component: id.as_str(),
                        trigger: BackpressureTrigger::TimeLagTooHigh {
                            threshold: max_time_lag,
                            actual,
                        },
                    });
                }
            }
        }

        causes
    }

    #[cfg(test)]
    pub(crate) fn make_test_monitor(config: PipelineHealthConfig) -> Self {
        let (_tx, rx) = watch::channel(false);
        let (monitor, _) = Self::new(config, rx);
        monitor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ComponentId, PipelineHealthConfig};
    use std::time::Duration;
    use tokio::time::Instant;
    use zksync_os_observability::{ComponentHealthReporter, GenericComponentState};
    use zksync_os_types::BackpressureTrigger;

    fn make_health(seq: u64, ts: Option<u64>) -> ComponentHealth {
        ComponentHealth {
            state: GenericComponentState::Active,
            specific_state: "active",
            state_entered_at: Instant::now(),
            last_processed_block_number: Some(seq),
            last_processed_block_timestamp: ts,
            last_processed_block_at: None,
        }
    }

    fn block_config_with_block_lag(max_lag: u64) -> PipelineHealthConfig {
        PipelineHealthConfig {
            block_pipeline: crate::config::BlockPipelineCondition {
                max_block_lag: Some(max_lag),
                max_time_lag: None,
            },
            ..PipelineHealthConfig::default()
        }
    }

    fn block_config_with_time_lag(max_lag: Duration) -> PipelineHealthConfig {
        PipelineHealthConfig {
            block_pipeline: crate::config::BlockPipelineCondition {
                max_block_lag: None,
                max_time_lag: Some(max_lag),
            },
            ..PipelineHealthConfig::default()
        }
    }

    fn batch_config_with_block_lag(max_lag: u64) -> PipelineHealthConfig {
        PipelineHealthConfig {
            batch_pipeline: crate::config::BatchPipelineCondition {
                max_block_lag: Some(max_lag),
                max_time_lag: None,
            },
            ..PipelineHealthConfig::default()
        }
    }

    #[test]
    fn below_lag_threshold_no_trigger() {
        let config = block_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head=100, applier=95, lag=5 < 10
        let result = monitor.evaluate(ComponentId::BlockApplier, &make_health(95, None), 5, None);
        assert!(result.is_empty());
    }

    #[test]
    fn above_lag_threshold_triggers() {
        let config = block_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head=100, applier=85, lag=15 > 10
        let result = monitor.evaluate(ComponentId::BlockApplier, &make_health(85, None), 15, None);
        assert!(matches!(
            result.into_iter().next().map(|c| c.trigger),
            Some(BackpressureTrigger::BlockLagTooHigh {
                threshold: 10,
                actual: 15
            })
        ));
    }

    #[test]
    fn at_exact_threshold_no_trigger() {
        let config = block_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // lag == threshold: should NOT trigger (strictly greater than)
        let result = monitor.evaluate(ComponentId::BlockApplier, &make_health(90, None), 10, None);
        assert!(result.is_empty());
    }

    #[test]
    fn time_lag_triggers_when_exceeded() {
        let config = block_config_with_time_lag(Duration::from_secs(30));
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head_ts=1000, applier_ts=960, lag=40s > 30s; block_lag irrelevant (no max_block_lag)
        let result = monitor.evaluate(
            ComponentId::BlockApplier,
            &make_health(90, Some(960)),
            0,
            Some(1000),
        );
        assert!(matches!(
            result.into_iter().next().map(|c| c.trigger),
            Some(BackpressureTrigger::TimeLagTooHigh { .. })
        ));
    }

    #[test]
    fn time_lag_skipped_when_component_timestamp_zero() {
        let config = block_config_with_time_lag(Duration::from_secs(1));
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // component timestamp = None → unavailable, must not trigger
        let result = monitor.evaluate(
            ComponentId::BlockApplier,
            &make_health(90, None),
            0,
            Some(1000),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn time_lag_skipped_when_head_timestamp_zero() {
        let config = block_config_with_time_lag(Duration::from_secs(1));
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head timestamp = None → unavailable, must not trigger
        let result = monitor.evaluate(
            ComponentId::BlockApplier,
            &make_health(90, Some(900)),
            0,
            None,
        );
        assert!(result.is_empty());
    }

    #[test]
    fn no_condition_set_never_triggers() {
        let config = PipelineHealthConfig::default();
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        let result = monitor.evaluate(
            ComponentId::BlockApplier,
            &make_health(0, None),
            10_000,
            Some(999_999),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn evaluate_and_update_sets_accepting_when_no_causes() {
        let config = PipelineHealthConfig::default();
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        monitor.evaluate_and_update_with_head(100, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting
        );
    }

    #[tokio::test]
    async fn counter_does_not_increment_on_reason_change() {
        let config = block_config_with_block_lag(10);
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);
        let (reporter, rx) = ComponentHealthReporter::new("block_applier");
        monitor.register(ComponentId::BlockApplier, rx);

        // Transition 1: Accepting → NotAccepting
        reporter.record_processed(85, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));

        // Transition 2: Still NotAccepting (deeper lag)
        reporter.record_processed(80, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));

        // Transition 3: NotAccepting → Accepting
        reporter.record_processed(100, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting
        );
    }

    #[test]
    fn evaluate_collects_both_block_and_time_lag() {
        let config = PipelineHealthConfig {
            block_pipeline: crate::config::BlockPipelineCondition {
                max_block_lag: Some(10),
                max_time_lag: Some(Duration::from_secs(30)),
            },
            ..PipelineHealthConfig::default()
        };
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // block_lag=15 (pre-computed), time_lag=40s — both should be returned
        let health = make_health(85, Some(960));
        let causes = monitor.evaluate(ComponentId::BlockApplier, &health, 15, Some(1000));
        assert_eq!(causes.len(), 2);
    }

    #[test]
    fn mid_pipeline_lag_does_not_cascade_to_downstream() {
        // BlockExecutor=200, BlockCanonizer=195 (adjacent diff=5, within threshold),
        // BlockApplier=193 (adjacent diff from Canonizer=2, within threshold).
        // Without cascade fix, BlockApplier shows head-relative lag=7 and could be confused
        // with a direct threshold violation. With the adjacent-diff fix, BlockApplier lag=2
        // (from its upstream Canonizer) — does NOT trigger (threshold=10).
        // Neither component should trigger since both adjacent diffs are within threshold.
        let config = PipelineHealthConfig {
            block_pipeline: crate::config::BlockPipelineCondition {
                max_block_lag: Some(10),
                max_time_lag: None,
            },
            ..PipelineHealthConfig::default()
        };
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);

        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (canon_reporter, canon_rx) = ComponentHealthReporter::new("block_canonizer");
        let (apply_reporter, apply_rx) = ComponentHealthReporter::new("block_applier");
        exec_reporter.record_processed(200, None);
        canon_reporter.record_processed(195, None);
        apply_reporter.record_processed(193, None);

        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::BlockCanonizer, canon_rx);
        monitor.register(ComponentId::BlockApplier, apply_rx);
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::BlockCanonizer);
        monitor.register_adjacency(ComponentId::BlockCanonizer, ComponentId::BlockApplier);

        monitor.evaluate_and_update_with_head(200, None);
        // BlockApplier must NOT trigger (adjacent lag = 2, threshold = 10)
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting
        );
    }

    #[test]
    fn adjacent_lag_triggers_when_exceeds_threshold() {
        // BlockCanonizer=200, BlockApplier=185 → adjacent lag=15 > threshold=10 → triggers
        let config = PipelineHealthConfig {
            block_pipeline: crate::config::BlockPipelineCondition {
                max_block_lag: Some(10),
                max_time_lag: None,
            },
            ..PipelineHealthConfig::default()
        };
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);

        let (canon_reporter, canon_rx) = ComponentHealthReporter::new("block_canonizer");
        let (apply_reporter, apply_rx) = ComponentHealthReporter::new("block_applier");
        canon_reporter.record_processed(200, None);
        apply_reporter.record_processed(185, None);

        monitor.register(ComponentId::BlockCanonizer, canon_rx);
        monitor.register(ComponentId::BlockApplier, apply_rx);
        monitor.register_adjacency(ComponentId::BlockCanonizer, ComponentId::BlockApplier);

        monitor.evaluate_and_update_with_head(200, None);
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));
    }

    #[test]
    fn adjacent_block_diff_computed_correctly() {
        let config = PipelineHealthConfig::default();
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);

        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (apply_reporter, apply_rx) = ComponentHealthReporter::new("block_applier");
        exec_reporter.record_processed(100, None);
        apply_reporter.record_processed(90, None);

        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::BlockApplier, apply_rx);
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::BlockApplier);

        let diffs = monitor.compute_adjacent_diffs();
        assert_eq!(diffs.get(&ComponentId::BlockApplier), Some(&10u64));
    }

    #[test]
    fn batch_block_lag_triggers_when_exceeded() {
        let config = batch_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head=100, batcher=85, lag=15 > threshold=10 → must trigger
        let causes = monitor.evaluate(ComponentId::Batcher, &make_health(85, None), 15, None);
        assert!(
            matches!(
                causes.into_iter().next().map(|c| c.trigger),
                Some(BackpressureTrigger::BlockLagTooHigh {
                    threshold: 10,
                    actual: 15
                })
            ),
            "batch block-lag must trigger when exceeded"
        );
    }

    #[test]
    fn batch_block_lag_no_trigger_below_threshold() {
        let config = batch_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        let causes = monitor.evaluate(ComponentId::Batcher, &make_health(95, None), 5, None);
        assert!(causes.is_empty());
    }

    #[test]
    fn batch_block_lag_no_trigger_at_exact_threshold() {
        let config = batch_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // lag==threshold: strictly greater-than required — must NOT trigger
        let causes = monitor.evaluate(ComponentId::Batcher, &make_health(90, None), 10, None);
        assert!(causes.is_empty());
    }

    #[tokio::test]
    async fn batch_block_lag_fires_in_evaluate_and_update() {
        let config = batch_config_with_block_lag(10);
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);
        let (reporter, rx) = ComponentHealthReporter::new("batcher");
        reporter.record_processed(85, None);
        monitor.register(ComponentId::Batcher, rx);

        monitor.evaluate_and_update_with_head(100, None);
        assert!(
            matches!(
                *monitor.acceptance_tx.borrow(),
                TransactionAcceptanceState::NotAccepting(_)
            ),
            "batch block-lag must set NotAccepting when lag > threshold"
        );
    }

    #[tokio::test]
    async fn batch_block_lag_high_watermark_prevents_false_positive() {
        // Simulates FriJobManager-style out-of-order reporting:
        // batch 2 (block 200) completes before batch 1 (block 100).
        // After the high-watermark guard, the reporter holds block 200
        // and ignores the late stale report of block 100.
        let config = batch_config_with_block_lag(50);
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);
        let (reporter, rx) = ComponentHealthReporter::new("fri_job_manager");
        monitor.register(ComponentId::FriJobManager, rx);

        // Batch 2 finishes first (higher block number).
        reporter.record_processed(200, None);
        // Batch 1 finishes late (lower block number) — must be ignored by high-watermark.
        reporter.record_processed(100, None);

        // head=210, watermark=200, lag=10 < threshold=50 → must NOT trigger.
        monitor.evaluate_and_update_with_head(210, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting,
            "high-watermark must prevent false backpressure from stale out-of-order report"
        );
    }

    #[tokio::test]
    async fn fri_job_manager_block_lag_triggers_and_clears() {
        let config = batch_config_with_block_lag(10);
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);
        let (reporter, rx) = ComponentHealthReporter::new("fri_job_manager");
        monitor.register(ComponentId::FriJobManager, rx);

        reporter.record_processed(85, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert!(
            matches!(
                *monitor.acceptance_tx.borrow(),
                TransactionAcceptanceState::NotAccepting(_)
            ),
            "FriJobManager lag=15 must trigger backpressure with threshold=10"
        );

        // FriJobManager catches up — backpressure must clear.
        reporter.record_processed(100, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting,
            "FriJobManager catching up must clear backpressure"
        );
    }

    #[tokio::test]
    async fn fri_job_manager_out_of_order_does_not_reinstate_backpressure() {
        // Two concurrent FRI provers: A finishes batch at block 200 first,
        // B finishes batch at block 190 second. Head is 210, threshold 50.
        // With high-watermark: B's report (190 < 200) is discarded;
        // stored block stays 200, lag=10 < 50 → must NOT trigger.
        let config = batch_config_with_block_lag(50);
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);
        let (reporter, rx) = ComponentHealthReporter::new("fri_job_manager");
        monitor.register(ComponentId::FriJobManager, rx);

        reporter.record_processed(200, None);
        reporter.record_processed(190, None); // stale — must be ignored

        monitor.evaluate_and_update_with_head(210, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting,
            "stale out-of-order report from slow prover must not trigger backpressure"
        );
    }

    #[test]
    #[should_panic(expected = "adjacency downstream component")]
    fn unregistered_adjacency_component_panics_in_compute() {
        // BlockCanonizer is in an adjacency pair but never registered —
        // compute_adjacent_diffs must panic rather than silently produce diff = up_seq.
        let config = PipelineHealthConfig::default();
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);

        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        exec_reporter.record_processed(100, None);
        monitor.register(ComponentId::BlockExecutor, exec_rx);
        // BlockCanonizer intentionally NOT registered.
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::BlockCanonizer);

        // Must panic because BlockCanonizer is absent from seqs.
        monitor.compute_adjacent_diffs();
    }

    #[tokio::test]
    async fn fri_job_manager_many_out_of_order_converge_to_highest() {
        // Multiple provers finishing batches in scrambled order.
        // High watermark must track the maximum across all reports.
        let (reporter, rx) = ComponentHealthReporter::new("fri_job_manager");

        for block in [50u64, 120, 80, 200, 90] {
            reporter.record_processed(block, None);
        }

        assert_eq!(
            rx.borrow().last_processed_block_number,
            Some(200),
            "high watermark must be the maximum of all reported block numbers"
        );
    }

    #[tokio::test]
    async fn fri_job_manager_backpressure_cause_identifies_component() {
        let config = batch_config_with_block_lag(10);
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);
        let (reporter, rx) = ComponentHealthReporter::new("fri_job_manager");
        monitor.register(ComponentId::FriJobManager, rx);

        reporter.record_processed(85, None);
        monitor.evaluate_and_update_with_head(100, None);

        if let TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure { causes },
        ) = &*monitor.acceptance_tx.borrow()
        {
            assert_eq!(causes.len(), 1);
            assert_eq!(causes[0].component, "fri_job_manager");
            assert!(matches!(
                causes[0].trigger,
                BackpressureTrigger::BlockLagTooHigh {
                    threshold: 10,
                    actual: 15
                }
            ));
        } else {
            panic!("expected NotAccepting with PipelineBackpressure cause");
        }
    }
}
