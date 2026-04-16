use crate::adjacent::{PipelineMaps, compute_adjacent_snapshots};
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

        // Fan-in guard: each downstream component must appear in at most one adjacency pair.
        // compute_adjacent_snapshots asserts the same invariant, but that function is also called
        // from the HTTP handler where a panic is not acceptable. Asserting here — a safe panic
        // site — ensures any wiring error is caught at startup, so the HTTP handler can rely on
        // the adjacency being fan-in-free.
        {
            let mut seen = std::collections::HashSet::new();
            for &(_, down) in &self.adjacency {
                assert!(
                    seen.insert(down),
                    "fan-in topology detected: downstream component {:?} appears in multiple \
                     adjacency pairs; the pipeline must be a linear chain with each component \
                     having at most one upstream neighbour",
                    down
                );
            }
        }

        // BlockExecutor must be registered: it is the head-of-pipeline source of truth used by
        // head_state() on every evaluation tick. Fail fast here rather than panicking in the loop.
        assert!(
            self.components
                .iter()
                .any(|(id, _)| *id == ComponentId::BlockExecutor),
            "PipelineHealthMonitor::run called without BlockExecutor registered; \
             BlockExecutor is the required head-of-pipeline source of truth — \
             call register() for BlockExecutor before run()"
        );

        // Every registered component that has at least one backpressure threshold must appear
        // as the downstream in an adjacency pair — unless it is BlockExecutor itself (the head,
        // which has no upstream by definition and whose self-lag is always 0).
        //
        // Without this guard a monitored component that is accidentally wired outside the
        // .pipe() chain would silently fall back to head-relative lag, which accumulates the
        // entire pipeline delay and triggers false backpressure immediately.
        {
            let downstream_set: std::collections::HashSet<ComponentId> =
                self.adjacency.iter().map(|&(_, down)| down).collect();
            for &(id, _) in &self.components {
                if id == ComponentId::BlockExecutor {
                    continue;
                }
                let cond = self.config.condition_for(id);
                let has_threshold = cond.max_block_lag.is_some()
                    || cond.max_time_lag.is_some()
                    || cond.max_batch_lag.is_some();
                assert!(
                    !has_threshold || downstream_set.contains(&id),
                    "component {:?} has backpressure thresholds but no adjacency pair registered; \
                     call register_adjacency() so the monitor can measure lag against its direct \
                     upstream rather than falling back to head-relative lag",
                    id
                );
            }
        }

        // Guard against a race where stop is already set before run() is entered.
        // changed() only waits for the *next* change, so without this check the monitor
        // would hang indefinitely if the sender was already dropped or set to true.
        if *self.stop_receiver.borrow_and_update() {
            return;
        }

        // Log startup summary: registered components, adjacency pairs, and effective thresholds.
        // This is the single most useful log for confirming correct wiring before a test run.
        tracing::info!(
            "PipelineHealthMonitor starting: {} components, {} adjacency pairs, metrics_interval={:?}",
            self.components.len(),
            self.adjacency.len(),
            self.config.metrics_interval,
        );
        for (id, _) in &self.components {
            let cond = self.config.condition_for(*id);
            tracing::debug!(
                "PipelineHealthMonitor: component {} threshold — max_block_lag={:?}, max_time_lag={:?}, max_batch_lag={:?}",
                id.as_str(),
                cond.max_block_lag,
                cond.max_time_lag,
                cond.max_batch_lag,
            );
        }
        for &(up, down) in &self.adjacency {
            tracing::debug!(
                "PipelineHealthMonitor: adjacency pair {} → {}",
                up.as_str(),
                down.as_str(),
            );
        }

        // Snapshot current state immediately so operators see accurate lag at monitor startup
        // rather than waiting up to metrics_interval for the first periodic tick.
        // WatchStream::from_changes skips the initial value, so without this call the monitor
        // would silently report 0 lag for up to 5 s even if the pipeline is already behind
        // (e.g. during replay from block 1).
        self.evaluate_and_update();

        // Build a merged stream of all component health changes.
        // WatchStream::from_changes only yields on subsequent changes; the periodic
        // metrics_tick is the safety net for components that do not produce change events.
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
                    h.last_processed
                        .as_ref()
                        .map(|c| c.block_number)
                        .unwrap_or(0),
                    h.last_processed.as_ref().and_then(|c| c.timestamp),
                )
            }
            None => {
                panic!(
                    "PipelineHealthMonitor: BlockExecutor is not registered; \
                     BlockExecutor is the required head-of-pipeline source of truth — \
                     call register() for BlockExecutor before run()"
                );
            }
        }
    }

    fn evaluate_and_update(&self) {
        let (head_seq, head_ts) = self.head_state();
        self.evaluate_and_update_with_head(head_seq, head_ts);
    }

    pub(crate) fn evaluate_and_update_with_head(&self, head_seq: u64, head_ts: Option<u64>) {
        // Snapshot processed/picked coordinates via PipelineMaps (shared with pipeline.rs).
        let maps = PipelineMaps::snapshot(&self.components);

        // Monitor-specific snapshots: explicit last_batch_picked (no fallback) for observational
        // metrics, and in-flight ranges for FriJobManager/SnarkJobManager.
        let mut last_batch_picked_snapshot: std::collections::HashMap<ComponentId, u64> =
            std::collections::HashMap::new();
        let mut in_flight_snapshot: std::collections::HashMap<ComponentId, (u64, u64)> =
            std::collections::HashMap::new();

        for (id, rx) in &self.components {
            let h = rx.borrow();
            if let Some(bp) = h.last_batch_picked {
                last_batch_picked_snapshot.insert(*id, bp);
            }
            if let (Some(first), Some(last)) = (&h.in_flight_first, &h.in_flight_last) {
                in_flight_snapshot.insert(*id, (first.batch_number, last.batch_number));
            }
        }

        // Compute adjacent block and time diffs. Using adjacent diff (upstream.last_processed −
        // downstream.last_picked) instead of head-relative lag prevents cascade false-positives:
        // a mid-pipeline bottleneck should not cause all downstream components to appear as
        // independent backpressure sources. This formula gives pure channel occupancy.
        // Note: Prometheus `component_block_lag` and `component_time_lag_seconds` still use
        // head-relative values for operator observability.
        let adjacent = compute_adjacent_snapshots(
            &self.adjacency,
            &maps.processed,
            &maps.picked,
            &maps.batch_processed,
            &maps.batch_picked,
        );

        // Log per-component lag snapshot at debug level so operators can watch
        // individual component lag values during testing without spamming info.
        if tracing::enabled!(tracing::Level::DEBUG) {
            for (id, _) in &self.components {
                let adj = adjacent.get(id);
                let block_diff = adj.map(|s| s.block_diff).unwrap_or(0);
                let time_diff_secs = adj
                    .and_then(|s| s.time_diff)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let batch_diff = adj.and_then(|s| s.batch_diff);
                let cond = self.config.condition_for(*id);
                let block_threshold = cond.max_block_lag;
                let batch_threshold = cond.max_batch_lag;
                tracing::debug!(
                    "pipeline lag snapshot: component={} block_diff={} block_threshold={:?} time_diff_secs={:.1} batch_diff={:?} batch_threshold={:?}",
                    id.as_str(),
                    block_diff,
                    block_threshold,
                    time_diff_secs,
                    batch_diff,
                    batch_threshold,
                );
            }
        }

        let mut active_component_ids: std::collections::HashSet<ComponentId> =
            std::collections::HashSet::new();
        let mut active_causes: Vec<BackpressureCause> = self
            .components
            .iter()
            .flat_map(|(id, _rx)| {
                let adj = adjacent.get(id);

                // block_lag: adjacent diff (upstream_seq − comp_seq).
                // 0 for BlockExecutor (the head — no upstream by definition, self-lag is always 0)
                // and for unmonitored components (which have no thresholds and can never trigger).
                // The startup assert in run() guarantees every other registered component
                // with a threshold has an adjacency pair, so adj is always Some for them.
                let block_lag = adj.map(|s| s.block_diff).unwrap_or(0);

                // time_lag: adjacent diff (upstream_ts − comp_ts); None when timestamps are
                // unavailable. Same rationale as block_lag: 0/None is correct for BlockExecutor
                // and unmonitored components; monitored non-head components are guaranteed by
                // the startup assert to have adjacency.
                let time_lag = adj.and_then(|s| s.time_diff);

                let batch_lag = adj.and_then(|s| s.batch_diff);
                let causes = self.evaluate(*id, block_lag, time_lag, batch_lag);
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
                // Cause set changed while already NotAccepting — log at debug for visibility.
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
        // Re-borrowing here would risk divergence: a component could update its watch value
        // between the acceptance commit and this loop, causing metric values to contradict
        // the acceptance state that was just published.
        for (id, _rx) in &self.components {
            let &(comp_seq, comp_ts) = maps
                .processed
                .get(id)
                .expect("id came from self.components");
            MONITOR_METRICS.backpressure_active[id].set(active_component_ids.contains(id) as u64);
            MONITOR_METRICS.component_last_processed_block[id].set(comp_seq);
            MONITOR_METRICS.component_block_lag[id].set(head_seq.saturating_sub(comp_seq));
            let time_lag = match (comp_ts, head_ts) {
                (Some(comp_ts), Some(h_ts)) => h_ts.saturating_sub(comp_ts) as f64,
                _ => 0.0,
            };
            MONITOR_METRICS.component_time_lag_seconds[id].set(time_lag);
        }

        for (&id, snap) in &adjacent {
            MONITOR_METRICS.component_block_diff_to_upstream[&id].set(snap.block_diff);
            let time_diff_secs = snap.time_diff.map(|d| d.as_secs_f64()).unwrap_or(0.0);
            MONITOR_METRICS.component_time_diff_to_upstream_seconds[&id].set(time_diff_secs);
            if let Some(batch_diff) = snap.batch_diff {
                MONITOR_METRICS.component_batch_diff_to_upstream[&id].set(batch_diff);
            }
        }

        // Absolute batch position metrics — informational, captured from the same snapshot
        // built above. Not used in backpressure decisions; divergence from acceptance state
        // is acceptable here.
        for (&id, &bn) in &maps.batch_processed {
            MONITOR_METRICS.component_last_processed_batch[&id].set(bn);
        }
        for (&id, &bp) in &last_batch_picked_snapshot {
            MONITOR_METRICS.component_last_picked_batch[&id].set(bp);
        }

        // In-flight prover metrics — only FriJobManager and SnarkJobManager call
        // record_in_flight_range(); emitting 0 for both when the queue is empty lets
        // operators distinguish "idle" from "stale gauge".
        for (id, _) in &self.components {
            if !matches!(
                id,
                ComponentId::FriJobManager | ComponentId::SnarkJobManager
            ) {
                continue;
            }
            let (first, last, count) =
                if let Some(&(first_bn, last_bn)) = in_flight_snapshot.get(id) {
                    (first_bn, last_bn, last_bn.saturating_sub(first_bn) + 1)
                } else {
                    (0, 0, 0)
                };
            MONITOR_METRICS.in_flight_first_batch[id].set(first);
            MONITOR_METRICS.in_flight_last_batch[id].set(last);
            MONITOR_METRICS.in_flight_batch_count[id].set(count);
        }
    }

    /// Evaluate backpressure for one component.
    ///
    /// `block_lag`: adjacent diff (upstream_seq − component_seq). 0 for BlockExecutor (the head)
    /// and unmonitored components. The startup assert guarantees every other monitored component
    /// has an adjacency pair, so this is always the true per-hop diff for those.
    ///
    /// `time_lag`: adjacent time diff (upstream_ts − component_ts); None when timestamps are
    /// unavailable or the component has no upstream.
    pub(crate) fn evaluate(
        &self,
        id: ComponentId,
        block_lag: u64,
        time_lag: Option<Duration>,
        batch_lag: Option<u64>,
    ) -> Vec<BackpressureCause> {
        let condition = self.config.condition_for(id);
        let mut causes = Vec::new();

        if let Some(max_lag) = condition.max_block_lag {
            // block_lag is pre-computed by caller: adjacent diff (upstream_seq − component_seq).
            // The startup assert guarantees every monitored component with a threshold has an
            // adjacency pair, so this is always the true per-hop channel occupancy.
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

        if let (Some(max_time_lag), Some(actual)) = (condition.max_time_lag, time_lag) {
            // time_lag is pre-computed by caller: adjacent diff (upstream_ts − component_ts).
            // None when timestamps are unavailable or the component has no upstream.
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

        if let (Some(max_batch), Some(actual)) = (condition.max_batch_lag, batch_lag)
            && actual > max_batch
        {
            causes.push(BackpressureCause {
                component: id.as_str(),
                trigger: BackpressureTrigger::BatchLagTooHigh {
                    threshold: max_batch,
                    actual,
                },
            });
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
    use zksync_os_observability::ComponentHealthReporter;
    use zksync_os_types::BackpressureTrigger;

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
                max_batch_lag: None,
            },
            ..PipelineHealthConfig::default()
        }
    }

    #[test]
    fn below_lag_threshold_no_trigger() {
        let config = block_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head=100, applier=95, lag=5 < 10
        let result = monitor.evaluate(ComponentId::BlockApplier, 5, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn above_lag_threshold_triggers() {
        let config = block_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head=100, applier=85, lag=15 > 10
        let result = monitor.evaluate(ComponentId::BlockApplier, 15, None, None);
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
        let result = monitor.evaluate(ComponentId::BlockApplier, 10, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn time_lag_triggers_when_exceeded() {
        let config = block_config_with_time_lag(Duration::from_secs(30));
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // adjacent time lag = 40s > threshold 30s; block_lag irrelevant (no max_block_lag)
        let result = monitor.evaluate(
            ComponentId::BlockApplier,
            0,
            Some(Duration::from_secs(40)),
            None,
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
        // time_lag = None (timestamps unavailable) → must not trigger
        let result = monitor.evaluate(ComponentId::BlockApplier, 0, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn time_lag_skipped_when_head_timestamp_zero() {
        let config = block_config_with_time_lag(Duration::from_secs(1));
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head timestamp = None → unavailable, must not trigger
        let result = monitor.evaluate(ComponentId::BlockApplier, 0, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn no_condition_set_never_triggers() {
        let config = PipelineHealthConfig::default();
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        let result = monitor.evaluate(
            ComponentId::BlockApplier,
            10_000,
            Some(Duration::from_secs(999_999)),
            None,
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
        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (reporter, rx) = ComponentHealthReporter::new("block_applier");
        exec_reporter.record_processed(100, None);
        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::BlockApplier, rx);
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::BlockApplier);

        // Transition 1: Accepting → NotAccepting
        // block diff = exec.last_processed(100) − applier.last_picked(85) = 15 > 10 → triggers
        reporter.record_processed(85, None);
        reporter.record_picked(85, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));

        // Transition 2: Still NotAccepting (deeper lag)
        // block diff = exec.last_processed(100) − applier.last_picked(80) = 20 > 10 → triggers
        reporter.record_processed(80, None);
        reporter.record_picked(80, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));

        // Transition 3: NotAccepting → Accepting
        // block diff = exec.last_processed(100) − applier.last_picked(100) = 0 → clears
        reporter.record_processed(100, None);
        reporter.record_picked(100, None);
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
        // Canonizer: processed 195, picked 195 → diff from Executor = 200 − 195 = 5 (within threshold=10)
        canon_reporter.record_processed(195, None);
        canon_reporter.record_picked(195, None);
        // Applier: processed 193, picked 193 → diff from Canonizer = 195 − 193 = 2 (within threshold=10)
        apply_reporter.record_processed(193, None);
        apply_reporter.record_picked(193, None);

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
        // Canonizer.last_processed=200, Applier.last_picked=185 → diff=15 > threshold=10 → triggers
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
        apply_reporter.record_picked(185, None);

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
        use crate::adjacent::compute_adjacent_snapshots;
        use std::collections::HashMap;

        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, (100u64, None));
        snapshots.insert(ComponentId::BlockApplier, (90u64, None));
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockApplier)];

        let result = compute_adjacent_snapshots(
            &adjacency,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(result[&ComponentId::BlockApplier].block_diff, 10);
    }

    #[test]
    fn batch_block_lag_triggers_when_exceeded() {
        let config = batch_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head=100, batcher=85, lag=15 > threshold=10 → must trigger
        let causes = monitor.evaluate(ComponentId::Batcher, 15, None, None);
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
        let causes = monitor.evaluate(ComponentId::Batcher, 5, None, None);
        assert!(causes.is_empty());
    }

    #[test]
    fn batch_block_lag_no_trigger_at_exact_threshold() {
        let config = batch_config_with_block_lag(10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // lag==threshold: strictly greater-than required — must NOT trigger
        let causes = monitor.evaluate(ComponentId::Batcher, 10, None, None);
        assert!(causes.is_empty());
    }

    #[tokio::test]
    async fn batch_block_lag_fires_in_evaluate_and_update() {
        let config = batch_config_with_block_lag(10);
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (reporter, rx) = ComponentHealthReporter::new("batcher");
        exec_reporter.record_processed(100, None);
        // block diff = exec.last_processed(100) − batcher.last_picked(85) = 15 > 10 → triggers
        reporter.record_processed(85, None);
        reporter.record_picked(85, None);
        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::Batcher, rx);
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::Batcher);

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
        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (reporter, rx) = ComponentHealthReporter::new("fri_job_manager");
        exec_reporter.record_processed(210, None);
        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::FriJobManager, rx);
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::FriJobManager);

        // Batch 2 finishes first (higher block number) — also picks up through block 200.
        reporter.record_processed(200, None);
        reporter.record_picked(200, None);
        // Batch 1 finishes late (lower block number) — must be ignored by high-watermark.
        reporter.record_processed(100, None);
        // record_picked(100) would be rejected by watermark guard (100 < 200) — no call needed.

        // head=210, watermark=200, block diff = exec.last_processed(210) − fri.last_picked(200) = 10 < 50
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
        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (reporter, rx) = ComponentHealthReporter::new("fri_job_manager");
        exec_reporter.record_processed(100, None);
        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::FriJobManager, rx);
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::FriJobManager);

        // block diff = exec.last_processed(100) − fri.last_picked(85) = 15 > 10 → triggers
        reporter.record_processed(85, None);
        reporter.record_picked(85, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert!(
            matches!(
                *monitor.acceptance_tx.borrow(),
                TransactionAcceptanceState::NotAccepting(_)
            ),
            "FriJobManager lag=15 must trigger backpressure with threshold=10"
        );

        // FriJobManager catches up — block diff = 100 − 100 = 0 → clears backpressure.
        reporter.record_processed(100, None);
        reporter.record_picked(100, None);
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
        // stored block stays 200, block diff = 210 − 200 = 10 < 50 → must NOT trigger.
        let config = batch_config_with_block_lag(50);
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (reporter, rx) = ComponentHealthReporter::new("fri_job_manager");
        exec_reporter.record_processed(210, None);
        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::FriJobManager, rx);
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::FriJobManager);

        // Prover A finishes first at block 200.
        reporter.record_processed(200, None);
        reporter.record_picked(200, None);
        // Prover B's stale report (190 < 200) is discarded by high-watermark guard.
        reporter.record_processed(190, None); // stale — must be ignored

        monitor.evaluate_and_update_with_head(210, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting,
            "stale out-of-order report from slow prover must not trigger backpressure"
        );
    }

    #[tokio::test]
    #[should_panic(expected = "BlockExecutor is the required head-of-pipeline source of truth")]
    async fn run_panics_when_block_executor_not_registered() {
        // A pipeline with only BlockApplier registered (no BlockExecutor) must fail fast
        // in run(), not crash inside the hot evaluation loop.
        let config = PipelineHealthConfig::default();
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (mut monitor, _) = PipelineHealthMonitor::new(config, stop_rx);

        let (reporter, rx) = ComponentHealthReporter::new("block_applier");
        reporter.record_processed(10, None);
        monitor.register(ComponentId::BlockApplier, rx);

        monitor.run().await;
    }

    #[tokio::test]
    #[should_panic(expected = "has backpressure thresholds but no adjacency pair registered")]
    async fn run_panics_when_monitored_component_has_no_adjacency() {
        // BlockApplier has a block-lag threshold but no adjacency pair registered.
        // run() must catch this at startup rather than silently falling back to
        // head-relative lag (which accumulates full pipeline delay, causing false backpressure).
        let config = PipelineHealthConfig {
            block_pipeline: crate::config::BlockPipelineCondition {
                max_block_lag: Some(10),
                max_time_lag: None,
            },
            ..PipelineHealthConfig::default()
        };
        let (_stop_tx, stop_rx) = watch::channel(false);
        let (mut monitor, _) = PipelineHealthMonitor::new(config, stop_rx);

        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (apply_reporter, apply_rx) = ComponentHealthReporter::new("block_applier");
        exec_reporter.record_processed(100, None);
        apply_reporter.record_processed(90, None);

        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::BlockApplier, apply_rx);
        // Intentionally no register_adjacency — must panic in run()

        monitor.run().await;
    }

    #[test]
    fn unregistered_adjacency_component_skips_pair_in_compute() {
        use crate::adjacent::compute_adjacent_snapshots;
        use std::collections::HashMap;

        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, (100u64, None));
        // BlockCanonizer intentionally absent from snapshots — pair is silently skipped.
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &adjacency,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(result.is_empty());
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
            rx.borrow().last_processed.as_ref().map(|c| c.block_number),
            Some(200),
            "high watermark must be the maximum of all reported block numbers"
        );
    }

    #[tokio::test]
    async fn fri_job_manager_backpressure_cause_identifies_component() {
        let config = batch_config_with_block_lag(10);
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (reporter, rx) = ComponentHealthReporter::new("fri_job_manager");
        exec_reporter.record_processed(100, None);
        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::FriJobManager, rx);
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::FriJobManager);

        // block diff = exec.last_processed(100) − fri.last_picked(85) = 15 > 10 → triggers
        reporter.record_processed(85, None);
        reporter.record_picked(85, None);
        monitor.evaluate_and_update_with_head(100, None);

        if let TransactionAcceptanceState::NotAccepting(reasons) = &*monitor.acceptance_tx.borrow()
        {
            assert_eq!(reasons.len(), 1);
            if let NotAcceptingReason::PipelineBackpressure { causes } = &reasons[0] {
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
                panic!("expected PipelineBackpressure reason");
            }
        } else {
            panic!("expected NotAccepting with PipelineBackpressure cause");
        }
    }

    #[test]
    fn mid_pipeline_time_lag_does_not_cascade_to_downstream() {
        // Formula: time_diff = upstream.last_processed_ts − downstream.last_picked_ts
        //
        // BlockExecutor processed ts=2000.
        // BlockCanonizer picked ts=1950 → diff from Executor = 2000 − 1950 = 50s > threshold=30s → triggers.
        // BlockApplier picked ts=1940 → diff from Canonizer.last_processed_ts(1950) = 10s ≤ 30s → no trigger.
        //
        // Without adjacent fix, BlockApplier head-relative lag = 2000−1940 = 60s > 30s → false positive.
        // With adjacent fix, BlockApplier sees lag = 1950−1940 = 10s ≤ 30s → no trigger.
        let config = PipelineHealthConfig {
            block_pipeline: crate::config::BlockPipelineCondition {
                max_block_lag: None,
                max_time_lag: Some(Duration::from_secs(30)),
            },
            ..PipelineHealthConfig::default()
        };
        let mut monitor = PipelineHealthMonitor::make_test_monitor(config);

        let (exec_reporter, exec_rx) = ComponentHealthReporter::new("block_executor");
        let (canon_reporter, canon_rx) = ComponentHealthReporter::new("block_canonizer");
        let (apply_reporter, apply_rx) = ComponentHealthReporter::new("block_applier");
        exec_reporter.record_processed(200, Some(2000));
        // Canonizer picked ts=1950 → upstream diff = 2000 − 1950 = 50s → triggers
        canon_reporter.record_processed(195, Some(1950));
        canon_reporter.record_picked(195, Some(1950));
        // Applier picked ts=1940 → adjacent diff from Canonizer.processed(1950) = 10s → no trigger
        apply_reporter.record_processed(193, Some(1940));
        apply_reporter.record_picked(193, Some(1940));

        monitor.register(ComponentId::BlockExecutor, exec_rx);
        monitor.register(ComponentId::BlockCanonizer, canon_rx);
        monitor.register(ComponentId::BlockApplier, apply_rx);
        monitor.register_adjacency(ComponentId::BlockExecutor, ComponentId::BlockCanonizer);
        monitor.register_adjacency(ComponentId::BlockCanonizer, ComponentId::BlockApplier);

        monitor.evaluate_and_update_with_head(200, Some(2000));

        // Only BlockCanonizer should trigger (50s adjacent lag > 30s threshold).
        // BlockApplier must NOT trigger despite 60s head-relative lag.
        match &*monitor.acceptance_tx.borrow() {
            TransactionAcceptanceState::NotAccepting(reasons) => {
                assert_eq!(reasons.len(), 1, "expected exactly one NotAcceptingReason");
                if let NotAcceptingReason::PipelineBackpressure { causes } = &reasons[0] {
                    assert_eq!(causes.len(), 1, "only BlockCanonizer should trigger");
                    assert_eq!(causes[0].component, "block_canonizer");
                } else {
                    panic!("expected PipelineBackpressure reason");
                }
            }
            other => panic!("expected NotAccepting with one cause, got {:?}", other),
        }
    }
}
