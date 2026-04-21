use crate::adjacent::{PipelineMaps, compute_adjacent_snapshots};
use crate::config::{BackpressureConfig, ComponentId};
use crate::metrics::MONITOR_METRICS;
use crate::pipeline_status::PipelineStatus;
use futures::stream::{StreamExt, select_all};
use reth_tasks::Runtime;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::WatchStream;
use zksync_os_observability::{ComponentState, ComponentStateReporter, GENERAL_METRICS};
use zksync_os_pipeline::PipelineMonitor;
use zksync_os_types::{
    BackpressureCause, BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState,
};

#[derive(Default)]
struct MonitorInner {
    components: Vec<(ComponentId, watch::Receiver<ComponentState>)>,
    edges: Vec<(ComponentId, ComponentId)>,
}

pub struct BackpressureMonitor {
    config: BackpressureConfig,
    inner: Arc<Mutex<MonitorInner>>,
    acceptance_tx: watch::Sender<TransactionAcceptanceState>,
    stop_receiver: watch::Receiver<bool>,
}

/// Builder-facing handle shared between `Pipeline::pipe()` and the monitor. Cloneable;
/// registration goes through this type and populates the monitor's `components`/`edges`.
#[derive(Clone)]
pub struct MonitorHandle {
    inner: Arc<Mutex<MonitorInner>>,
}

impl MonitorHandle {
    /// Snapshot of `(ComponentId, rx)` pairs for the status server's `/status/pipeline`
    /// and `/status/accepting` endpoints.
    pub fn component_state_entries(&self) -> Vec<(ComponentId, watch::Receiver<ComponentState>)> {
        self.inner
            .lock()
            .unwrap()
            .components
            .iter()
            .map(|(id, rx)| (*id, rx.clone()))
            .collect()
    }

    /// Snapshot of declared `(upstream, downstream)` edges. The status server consumes
    /// this so `/status/pipeline` adjacency matches the Prometheus / acceptance view
    /// computed by the monitor itself.
    pub fn edges(&self) -> Vec<(ComponentId, ComponentId)> {
        self.inner.lock().unwrap().edges.clone()
    }
}

impl PipelineMonitor for MonitorHandle {
    fn register(&self, id: ComponentId, upstream: Option<ComponentId>) -> ComponentStateReporter {
        let (reporter, rx) = ComponentStateReporter::new(id.as_str());
        let mut inner = self.inner.lock().unwrap();
        assert!(
            !inner.components.iter().any(|(cid, _)| *cid == id),
            "PipelineMonitor: component {id:?} already registered"
        );
        inner.components.push((id, rx));
        if let Some(up) = upstream {
            inner.edges.push((up, id));
        }
        reporter
    }
}

impl BackpressureMonitor {
    pub fn new(config: BackpressureConfig, stop_receiver: watch::Receiver<bool>) -> Self {
        const MIN_METRICS_INTERVAL: Duration = Duration::from_millis(10);
        assert!(
            config.metrics_interval >= MIN_METRICS_INTERVAL,
            "BackpressureConfig::metrics_interval must be >= {MIN_METRICS_INTERVAL:?} (got {:?})",
            config.metrics_interval,
        );
        let (acceptance_tx, _) = watch::channel(TransactionAcceptanceState::Accepting);
        Self {
            config,
            inner: Arc::new(Mutex::new(MonitorInner::default())),
            acceptance_tx,
            stop_receiver,
        }
    }

    pub fn handle(&self) -> MonitorHandle {
        MonitorHandle {
            inner: self.inner.clone(),
        }
    }

    /// Trait-object registrar for [`zksync_os_pipeline::Pipeline::new`]. Wraps [`MonitorHandle`]
    /// so the wiring site only juggles one `Arc<dyn PipelineMonitor>` instead of manually
    /// cloning the handle and erasing it.
    pub fn registrar(&self) -> Arc<dyn PipelineMonitor> {
        Arc::new(self.handle())
    }

    /// Finalize wiring: snapshot the adjacency graph, spawn [`Self::run`] on `runtime`, and
    /// return the [`PipelineStatus`] consumed by the tx-acceptance gate and status server.
    ///
    /// Call **after** `Pipeline::spawn()` and after any ad-hoc [`MonitorHandle::register`]
    /// calls outside the builder — the snapshot is taken here.
    pub fn spawn(self, runtime: &Runtime) -> PipelineStatus {
        let handle = self.handle();
        let component_states = Arc::new(handle.component_state_entries());
        let edges = Arc::new(handle.edges());
        let acceptance_rx = self.acceptance_tx.subscribe();
        runtime.spawn_critical_task("backpressure monitor", self.run());
        PipelineStatus {
            acceptance_rx,
            component_states,
            edges,
        }
    }

    /// Test-only registration helper. Production wiring goes through
    /// [`MonitorHandle::register`] via the pipeline builder, which supplies
    /// the real upstream for each edge. This helper infers a linear chain from
    /// registration order: each non-first component gets an edge from its
    /// predecessor, matching the shape tests actually exercise.
    #[cfg(test)]
    pub fn register_linear(&mut self, id: ComponentId, receiver: watch::Receiver<ComponentState>) {
        let mut inner = self.inner.lock().unwrap();
        let upstream = inner.components.last().map(|(id, _)| *id);
        if let Some(up) = upstream {
            inner.edges.push((up, id));
        }
        inner.components.push((id, receiver));
    }

    pub async fn run(mut self) {
        // Initial state is Accepting; set the gauge so it is correct before any transition fires.
        MONITOR_METRICS.accepting.set(1);

        // Prometheus metrics timer — independent of state evaluation.
        let mut metrics_tick = tokio::time::interval(self.config.metrics_interval);
        metrics_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Clone rather than drain: `MonitorHandle::{component_state_entries, edges}` reads
        // the same `inner` for the status server, and callers may snapshot those after
        // `run()` has already started. Draining here would silently empty those snapshots
        // depending on call order. Cloning is safe because registration is complete before
        // `run()` is invoked (pipeline builder finishes first), so there is no contention.
        let (components, edges) = {
            let inner = self.inner.lock().unwrap();
            (inner.components.clone(), inner.edges.clone())
        };

        // BlockExecutor is the pipeline head of truth: `head_state_of` looks it up by
        // ComponentId, and `evaluate_and_update_inner` uses its last_processed to compute
        // head-relative lag metrics. Registration order does not matter (lookup is by id),
        // but the component must be present — otherwise `head_state_of` would panic deep in
        // the hot evaluation loop on every tick.
        assert!(
            components
                .iter()
                .any(|(id, _)| *id == ComponentId::BlockExecutor),
            "BackpressureMonitor: BlockExecutor must be registered before run(); \
             it is the pipeline head used to compute head-relative lag. \
             Fix: wire BlockExecutor via MonitorHandle::register in the pipeline builder."
        );

        // Every declared edge must name an upstream that was registered. Catches typos at
        // register() call sites and prevents silent-zero adjacency lag when an upstream is
        // never wired.
        for (upstream, downstream) in &edges {
            assert!(
                components.iter().any(|(id, _)| id == upstream),
                "BackpressureMonitor: edge {upstream:?} -> {downstream:?} names upstream \
                 {upstream:?} which was never registered"
            );
        }

        // Guard against a race where stop is already set before run() is entered.
        // changed() only waits for the *next* change, so without this check the monitor
        // would hang indefinitely if the sender was already dropped or set to true.
        if *self.stop_receiver.borrow_and_update() {
            return;
        }

        // Log startup summary: registered components and effective thresholds.
        // This is the single most useful log for confirming correct wiring before a test run.
        // A component is "active" iff condition_for returns any Some threshold; a fresh
        // default config produces all-None thresholds and silently disables every component,
        // which the info-level line surfaces via the disabled names.
        let mut active = Vec::new();
        let mut disabled = Vec::new();
        for (id, _) in &components {
            let cond = self.config.condition_for(*id);
            let is_active = cond.max_block_diff_to_upstream.is_some()
                || cond.max_time_diff_to_upstream.is_some()
                || cond.max_batch_diff_to_upstream.is_some();
            if is_active {
                active.push(id.as_str());
            } else {
                disabled.push(id.as_str());
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
        tracing::info!(
            "BackpressureMonitor starting: active={}/{} components, metrics_interval={:?}, disabled={:?}",
            active.len(),
            components.len(),
            self.config.metrics_interval,
            disabled,
        );

        // Snapshot current state immediately so operators see accurate lag at monitor startup
        // rather than waiting up to metrics_interval for the first periodic tick.
        // WatchStream::from_changes skips the initial value, so without this call the monitor
        // would silently report 0 lag for up to 5 s even if the pipeline is already behind
        // (e.g. during replay from block 1).
        self.evaluate_and_update_inner(&components, &edges);

        // Build a merged stream of all component state changes.
        // WatchStream::from_changes only yields on subsequent changes; the periodic
        // metrics_tick is the safety net for components that do not produce change events.
        let streams = components
            .iter()
            .map(|(_, rx)| WatchStream::from_changes(rx.clone()))
            .collect::<Vec<_>>();
        let mut combined = select_all(streams);

        loop {
            tokio::select! {
                Some(_) = combined.next() => self.evaluate_and_update_inner(&components, &edges),
                _ = metrics_tick.tick() => self.evaluate_and_update_inner(&components, &edges),
                _ = self.stop_receiver.changed() => {
                    tracing::info!("BackpressureMonitor: stop signal received");
                    return;
                }
            }
        }
    }

    fn head_state_of(
        components: &[(ComponentId, watch::Receiver<ComponentState>)],
    ) -> (u64, Option<u64>) {
        match components
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
                    "BackpressureMonitor: BlockExecutor is not registered; \
                     BlockExecutor is the required head-of-pipeline source of truth — \
                     wire it via MonitorHandle::register before run()"
                );
            }
        }
    }

    fn evaluate_and_update_inner(
        &self,
        components: &[(ComponentId, watch::Receiver<ComponentState>)],
        edges: &[(ComponentId, ComponentId)],
    ) {
        let (head_block, head_ts) = Self::head_state_of(components);
        self.evaluate_and_update_with_head_inner(components, edges, head_block, head_ts);
    }

    /// Test-facing shim that snapshots `components` and `edges` from `inner` and delegates
    /// to the real evaluation function. Production `run()` snapshots the inner once at startup
    /// and passes the locals directly, avoiding per-tick lock contention.
    #[cfg(test)]
    pub(crate) fn evaluate_and_update_with_head(&self, head_block: u64, head_ts: Option<u64>) {
        let (components, edges) = {
            let inner = self.inner.lock().unwrap();
            (inner.components.clone(), inner.edges.clone())
        };
        self.evaluate_and_update_with_head_inner(&components, &edges, head_block, head_ts);
    }

    pub(crate) fn evaluate_and_update_with_head_inner(
        &self,
        components: &[(ComponentId, watch::Receiver<ComponentState>)],
        edges: &[(ComponentId, ComponentId)],
        head_block: u64,
        head_ts: Option<u64>,
    ) {
        // Snapshot processed/picked coordinates via PipelineMaps (shared with pipeline.rs).
        // maps.batch_picked applies the same fallback policy as block-level picked:
        // `last_batch_picked` → `batch_number` when the former is absent. The Prometheus
        // `component_last_picked_batch` gauge below is sourced from the raw watch state
        // (`h.last_batch_picked`, not maps.batch_picked) so the gauge remains a faithful
        // view of explicit batch picks only (i.e. `record_picked` calls with a batch arg).
        let maps = PipelineMaps::snapshot(components);

        // In-flight ranges — not part of PipelineMaps because they're only reported
        // by components that hold multiple items concurrently (prover managers and
        // L1 senders).
        let mut in_flight_snapshot: std::collections::HashMap<ComponentId, (u64, u64)> =
            std::collections::HashMap::new();

        for (id, rx) in components {
            let h = rx.borrow();
            if let (Some(first), Some(last)) = (&h.in_flight_first, &h.in_flight_last) {
                in_flight_snapshot.insert(*id, (first.batch_number, last.batch_number));
            }
        }

        // Compute adjacent block and time diffs. Using adjacent diff (upstream.last_processed −
        // downstream.last_picked) instead of head-relative lag prevents cascade false-positives:
        // a mid-pipeline bottleneck should not cause all downstream components to appear as
        // independent backpressure sources. This formula gives pure channel occupancy.
        // Note: Prometheus `component_block_diff_to_head` and `component_time_diff_to_head_seconds`
        // still use head-relative values for operator observability.
        let adjacent = compute_adjacent_snapshots(
            edges,
            &maps.processed,
            &maps.picked,
            &maps.batch_processed,
            &maps.batch_picked,
        );

        // Log per-component lag snapshot at debug level so operators can watch
        // individual component lag values during testing without spamming info.
        if tracing::enabled!(tracing::Level::DEBUG) {
            for (id, _) in components {
                let adj = adjacent.get(id);
                let block_diff = adj.map(|s| s.block_diff).unwrap_or(0);
                let time_diff_secs = adj
                    .and_then(|s| s.time_diff)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let batch_diff = adj.and_then(|s| s.batch_diff);
                let cond = self.config.condition_for(*id);
                let block_threshold = cond.max_block_diff_to_upstream;
                let batch_threshold = cond.max_batch_diff_to_upstream;
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
        let mut active_causes: Vec<BackpressureCause> = components
            .iter()
            .flat_map(|(id, _rx)| {
                let adj = adjacent.get(id);

                // block_diff_to_upstream: adjacent diff (upstream_block − comp_block).
                // 0 for BlockExecutor (the head — no upstream by definition, self-lag is always 0)
                // and for unmonitored components (which have no thresholds and can never trigger).
                // The startup assert in run() guarantees every other registered component
                // with a threshold has an adjacency pair, so adj is always Some for them.
                let block_diff_to_upstream = adj.map(|s| s.block_diff).unwrap_or(0);

                // time_diff_to_upstream: adjacent diff (upstream_ts − comp_ts); None when timestamps are
                // unavailable. Same rationale as block_diff_to_upstream: 0/None is correct for BlockExecutor
                // and unmonitored components; monitored non-head components are guaranteed by
                // the startup assert to have adjacency.
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
        let now = tokio::time::Instant::now();
        for (id, rx) in components {
            // A component absent from the map hasn't yet called record_processed; emit 0 so
            // the gauge is defined rather than stale from a prior tick.
            let (comp_block, comp_ts) = maps.processed.get(id).copied().unwrap_or((0, None));
            MONITOR_METRICS.component_order[id].set(id.pipeline_order());
            MONITOR_METRICS.backpressure_active[id].set(active_component_ids.contains(id) as u64);
            MONITOR_METRICS.component_last_processed_block[id].set(comp_block);
            MONITOR_METRICS.component_block_diff_to_head[id]
                .set(head_block.saturating_sub(comp_block));
            let time_diff_to_head = match (comp_ts, head_ts) {
                (Some(comp_ts), Some(h_ts)) => h_ts.saturating_sub(comp_ts) as f64,
                _ => 0.0,
            };
            MONITOR_METRICS.component_time_diff_to_head_seconds[id].set(time_diff_to_head);

            // State age — gauge complement to component_time_spent_in_state. The counter
            // only moves on transitions; this gauge keeps long-idle states visible in
            // Prometheus by re-publishing "seconds in current state" every tick.
            let h = rx.borrow();
            let age = now
                .saturating_duration_since(h.state_entered_at)
                .as_secs_f64();
            GENERAL_METRICS.component_state_age_seconds[&(id.as_str(), h.state, h.specific_state)]
                .set(age);
        }

        for (&id, snap) in &adjacent {
            MONITOR_METRICS.component_block_diff_to_upstream[&id].set(snap.block_diff);
            let time_diff_secs = snap.time_diff.map(|d| d.as_secs_f64()).unwrap_or(0.0);
            MONITOR_METRICS.component_time_diff_to_upstream_seconds[&id].set(time_diff_secs);
            if let Some(batch_diff) = snap.batch_diff {
                MONITOR_METRICS.component_batch_diff_to_upstream[&id].set(batch_diff);
            }
        }

        // Absolute batch position metrics — informational. Not used in backpressure decisions.
        // `component_last_picked_batch` is sourced from the raw watch state (not maps.batch_picked)
        // so the gauge reflects only explicit batch picks (via `record_picked` with a batch arg).
        // maps.batch_picked applies a fallback to `batch_number` for adjacency math, which
        // would conflate the two gauges if used here.
        for (&id, &bn) in &maps.batch_processed {
            MONITOR_METRICS.component_last_processed_batch[&id].set(bn);
        }
        for (id, rx) in components {
            if let Some(bp) = rx.borrow().last_batch_picked {
                MONITOR_METRICS.component_last_picked_batch[id].set(bp);
            }
        }

        // In-flight metrics — populated by components that hold multiple batches
        // concurrently (FriJobManager, SnarkJobManager, and the three L1 senders
        // while awaiting inclusion). Emitting 0 for both when the set is empty
        // lets operators distinguish "idle" from "stale gauge".
        for (id, _) in components {
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
    /// `block_diff_to_upstream`: adjacent diff (upstream_block − component_block). 0 for BlockExecutor (the head)
    /// and unmonitored components. The startup assert guarantees every other monitored component
    /// has an adjacency pair, so this is always the true per-hop diff for those.
    ///
    /// `time_diff_to_upstream`: adjacent time diff (upstream_ts − component_ts); None when timestamps are
    /// unavailable or the component has no upstream.
    pub(crate) fn evaluate(
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

    #[cfg(test)]
    pub(crate) fn make_test_monitor(config: BackpressureConfig) -> Self {
        let (_tx, rx) = watch::channel(false);
        Self::new(config, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackpressureConfig, ComponentId};
    use crate::metrics::MONITOR_METRICS;
    use std::time::Duration;
    use zksync_os_observability::ComponentStateReporter;
    use zksync_os_types::BackpressureTrigger;

    fn block_config_with_block_diff_to_upstream(max_diff: u64) -> BackpressureConfig {
        BackpressureConfig {
            block_pipeline: crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(max_diff),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
            },
            ..BackpressureConfig::default()
        }
    }

    fn block_config_with_time_diff_to_upstream(max_diff: Duration) -> BackpressureConfig {
        BackpressureConfig {
            block_pipeline: crate::config::PipelineCondition {
                max_block_diff_to_upstream: None,
                max_time_diff_to_upstream: Some(max_diff),
                max_batch_diff_to_upstream: None,
            },
            ..BackpressureConfig::default()
        }
    }

    fn batch_config_with_block_diff_to_upstream(max_diff: u64) -> BackpressureConfig {
        BackpressureConfig {
            batch_pipeline: crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(max_diff),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
            },
            ..BackpressureConfig::default()
        }
    }

    fn batch_config_with_batch_diff_to_upstream(max_diff: u64) -> BackpressureConfig {
        BackpressureConfig {
            batch_pipeline: crate::config::PipelineCondition {
                max_block_diff_to_upstream: None,
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: Some(max_diff),
            },
            ..BackpressureConfig::default()
        }
    }

    #[test]
    fn below_lag_threshold_no_trigger() {
        let config = block_config_with_block_diff_to_upstream(10);
        let monitor = BackpressureMonitor::make_test_monitor(config);
        let result = monitor.evaluate(ComponentId::BlockApplier, 5, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn above_lag_threshold_triggers() {
        let config = block_config_with_block_diff_to_upstream(10);
        let monitor = BackpressureMonitor::make_test_monitor(config);
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
        let config = block_config_with_block_diff_to_upstream(10);
        let monitor = BackpressureMonitor::make_test_monitor(config);
        // Strictly greater-than — equal-to-threshold must not trigger.
        let result = monitor.evaluate(ComponentId::BlockApplier, 10, None, None);
        assert!(result.is_empty());
    }

    #[test]
    fn time_diff_to_upstream_triggers_when_exceeded() {
        let config = block_config_with_time_diff_to_upstream(Duration::from_secs(30));
        let monitor = BackpressureMonitor::make_test_monitor(config);
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
        let monitor = BackpressureMonitor::make_test_monitor(config);
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
        let config = BackpressureConfig::default();
        let monitor = BackpressureMonitor::make_test_monitor(config);
        monitor.evaluate_and_update_with_head(100, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting
        );
    }

    #[test]
    fn evaluate_and_update_sets_component_order_metric() {
        let config = BackpressureConfig::default();
        let mut monitor = BackpressureMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (canon_reporter, canon_rx) = ComponentStateReporter::new("block_canonizer");
        let (apply_reporter, apply_rx) = ComponentStateReporter::new("block_applier");

        exec_reporter.record_processed(100, None, None);
        canon_reporter.record_processed(95, None, None);
        canon_reporter.record_picked(95, None, None);
        apply_reporter.record_processed(90, None, None);
        apply_reporter.record_picked(90, None, None);

        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::BlockCanonizer, canon_rx);
        monitor.register_linear(ComponentId::BlockApplier, apply_rx);

        monitor.evaluate_and_update_with_head(100, None);

        assert_eq!(
            MONITOR_METRICS.component_order[&ComponentId::BlockExecutor].get(),
            10
        );
        assert_eq!(
            MONITOR_METRICS.component_order[&ComponentId::BlockCanonizer].get(),
            20
        );
        assert_eq!(
            MONITOR_METRICS.component_order[&ComponentId::BlockApplier].get(),
            30
        );
    }

    #[tokio::test]
    async fn counter_does_not_increment_on_reason_change() {
        let config = block_config_with_block_diff_to_upstream(10);
        let mut monitor = BackpressureMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (reporter, rx) = ComponentStateReporter::new("block_applier");
        exec_reporter.record_processed(100, None, None);
        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::BlockApplier, rx);

        reporter.record_processed(85, None, None);
        reporter.record_picked(85, None, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));

        reporter.record_processed(80, None, None);
        reporter.record_picked(80, None, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));

        reporter.record_processed(100, None, None);
        reporter.record_picked(100, None, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting
        );
    }

    #[test]
    fn evaluate_collects_both_block_and_time_diff_to_upstream() {
        let config = BackpressureConfig {
            block_pipeline: crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(10),
                max_time_diff_to_upstream: Some(Duration::from_secs(30)),
                max_batch_diff_to_upstream: None,
            },
            ..BackpressureConfig::default()
        };
        let monitor = BackpressureMonitor::make_test_monitor(config);
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
        // Executor=200, Canonizer=195 (adjacent diff 5), Applier=193 (adjacent diff 2
        // from Canonizer). Both within threshold=10: Applier's head-relative lag of 7
        // must not trigger since backpressure is evaluated per-hop, not against head.
        let config = BackpressureConfig {
            block_pipeline: crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(10),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
            },
            ..BackpressureConfig::default()
        };
        let mut monitor = BackpressureMonitor::make_test_monitor(config);

        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (canon_reporter, canon_rx) = ComponentStateReporter::new("block_canonizer");
        let (apply_reporter, apply_rx) = ComponentStateReporter::new("block_applier");
        exec_reporter.record_processed(200, None, None);
        canon_reporter.record_processed(195, None, None);
        canon_reporter.record_picked(195, None, None);
        apply_reporter.record_processed(193, None, None);
        apply_reporter.record_picked(193, None, None);

        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::BlockCanonizer, canon_rx);
        monitor.register_linear(ComponentId::BlockApplier, apply_rx);

        monitor.evaluate_and_update_with_head(200, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting
        );
    }

    #[test]
    fn adjacent_lag_triggers_when_exceeds_threshold() {
        let config = BackpressureConfig {
            block_pipeline: crate::config::PipelineCondition {
                max_block_diff_to_upstream: Some(10),
                max_time_diff_to_upstream: None,
                max_batch_diff_to_upstream: None,
            },
            ..BackpressureConfig::default()
        };
        let mut monitor = BackpressureMonitor::make_test_monitor(config);

        let (canon_reporter, canon_rx) = ComponentStateReporter::new("block_canonizer");
        let (apply_reporter, apply_rx) = ComponentStateReporter::new("block_applier");
        canon_reporter.record_processed(200, None, None);
        apply_reporter.record_processed(185, None, None);
        apply_reporter.record_picked(185, None, None);

        monitor.register_linear(ComponentId::BlockCanonizer, canon_rx);
        monitor.register_linear(ComponentId::BlockApplier, apply_rx);

        monitor.evaluate_and_update_with_head(200, None);
        assert!(matches!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::NotAccepting(_)
        ));
    }

    #[test]
    fn batch_block_diff_to_upstream_triggers_when_exceeded() {
        let config = batch_config_with_block_diff_to_upstream(10);
        let monitor = BackpressureMonitor::make_test_monitor(config);
        let causes = monitor.evaluate(ComponentId::BatchVerification, 15, None, None);
        assert!(
            matches!(
                causes.into_iter().next().map(|c| c.trigger),
                Some(BackpressureTrigger::BlockDiffToUpstreamTooHigh {
                    threshold: 10,
                    actual: 15
                })
            ),
            "batch block-lag must trigger when exceeded"
        );
    }

    #[test]
    fn batch_block_diff_to_upstream_no_trigger_below_threshold() {
        let config = batch_config_with_block_diff_to_upstream(10);
        let monitor = BackpressureMonitor::make_test_monitor(config);
        let causes = monitor.evaluate(ComponentId::BatchVerification, 5, None, None);
        assert!(causes.is_empty());
    }

    #[test]
    fn batch_block_diff_to_upstream_no_trigger_at_exact_threshold() {
        let config = batch_config_with_block_diff_to_upstream(10);
        let monitor = BackpressureMonitor::make_test_monitor(config);
        // Strictly greater-than — equal-to-threshold must not trigger.
        let causes = monitor.evaluate(ComponentId::BatchVerification, 10, None, None);
        assert!(causes.is_empty());
    }

    #[tokio::test]
    async fn batch_block_diff_to_upstream_fires_in_evaluate_and_update() {
        let config = batch_config_with_block_diff_to_upstream(10);
        let mut monitor = BackpressureMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (reporter, rx) = ComponentStateReporter::new("batch_verification");
        exec_reporter.record_processed(100, None, None);
        reporter.record_processed(85, None, None);
        reporter.record_picked(85, None, None);
        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::BatchVerification, rx);

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
    async fn batch_block_diff_to_upstream_high_watermark_prevents_false_positive() {
        // FriJobManager-style out-of-order: batch 2 (block 200) completes before
        // batch 1 (block 100). The high-watermark guard must ignore the late
        // stale report so the stored block stays at 200.
        let config = batch_config_with_block_diff_to_upstream(50);
        let mut monitor = BackpressureMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (reporter, rx) = ComponentStateReporter::new("fri_job_manager");
        exec_reporter.record_processed(210, None, None);
        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::FriJobManager, rx);

        reporter.record_processed(200, None, None);
        reporter.record_picked(200, None, None);
        // Late stale report — the watermark guard must drop it.
        reporter.record_processed(100, None, None);

        monitor.evaluate_and_update_with_head(210, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting,
            "high-watermark must prevent false backpressure from stale out-of-order report"
        );
    }

    #[tokio::test]
    async fn fri_job_manager_block_diff_to_upstream_triggers_and_clears() {
        let config = batch_config_with_block_diff_to_upstream(10);
        let mut monitor = BackpressureMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (reporter, rx) = ComponentStateReporter::new("fri_job_manager");
        exec_reporter.record_processed(100, None, None);
        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::FriJobManager, rx);

        reporter.record_processed(85, None, None);
        reporter.record_picked(85, None, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert!(
            matches!(
                *monitor.acceptance_tx.borrow(),
                TransactionAcceptanceState::NotAccepting(_)
            ),
            "FriJobManager lag=15 must trigger backpressure with threshold=10"
        );

        reporter.record_processed(100, None, None);
        reporter.record_picked(100, None, None);
        monitor.evaluate_and_update_with_head(100, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting,
            "FriJobManager catching up must clear backpressure"
        );
    }

    #[tokio::test]
    async fn fri_job_manager_out_of_order_does_not_reinstate_backpressure() {
        // Two concurrent FRI provers completing out of order: B's older report must be
        // dropped by the high-watermark guard so A's newer value is not clobbered.
        let config = batch_config_with_block_diff_to_upstream(50);
        let mut monitor = BackpressureMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (reporter, rx) = ComponentStateReporter::new("fri_job_manager");
        exec_reporter.record_processed(210, None, None);
        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::FriJobManager, rx);

        reporter.record_processed(200, None, None);
        reporter.record_picked(200, None, None);
        reporter.record_processed(190, None, None); // stale — must be ignored

        monitor.evaluate_and_update_with_head(210, None);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting,
            "stale out-of-order report from slow prover must not trigger backpressure"
        );
    }

    #[tokio::test]
    #[should_panic(expected = "BlockExecutor must be registered")]
    async fn run_panics_when_block_executor_not_registered() {
        // A pipeline with only BlockApplier registered (no BlockExecutor) must fail fast
        // in run(), not crash inside the hot evaluation loop.
        let config = BackpressureConfig::default();
        let (_stop_tx, stop_rx) = watch::channel(false);
        let mut monitor = BackpressureMonitor::new(config, stop_rx);

        let (reporter, rx) = ComponentStateReporter::new("block_applier");
        reporter.record_processed(10, None, None);
        monitor.register_linear(ComponentId::BlockApplier, rx);

        monitor.run().await;
    }

    #[tokio::test]
    #[should_panic(expected = "names upstream")]
    async fn run_panics_on_edge_with_unregistered_upstream() {
        let config = BackpressureConfig::default();
        let (_stop_tx, stop_rx) = watch::channel(false);
        let monitor = BackpressureMonitor::new(config, stop_rx);

        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (apply_reporter, apply_rx) = ComponentStateReporter::new("block_applier");
        exec_reporter.record_processed(10, None, None);
        apply_reporter.record_processed(10, None, None);
        {
            let mut inner = monitor.inner.lock().unwrap();
            inner.components.push((ComponentId::BlockExecutor, exec_rx));
            inner.components.push((ComponentId::BlockApplier, apply_rx));
            inner
                .edges
                .push((ComponentId::BlockCanonizer, ComponentId::BlockApplier));
        }
        monitor.run().await;
    }

    #[tokio::test]
    async fn fri_job_manager_many_out_of_order_converge_to_highest() {
        // Multiple provers finishing batches in scrambled order.
        // High watermark must track the maximum across all reports.
        let (reporter, rx) = ComponentStateReporter::new("fri_job_manager");

        for block in [50u64, 120, 80, 200, 90] {
            reporter.record_processed(block, None, None);
        }

        assert_eq!(
            rx.borrow().last_processed.as_ref().map(|c| c.block_number),
            Some(200),
            "high watermark must be the maximum of all reported block numbers"
        );
    }

    #[tokio::test]
    async fn fri_job_manager_backpressure_cause_identifies_component() {
        let config = batch_config_with_block_diff_to_upstream(10);
        let mut monitor = BackpressureMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (reporter, rx) = ComponentStateReporter::new("fri_job_manager");
        exec_reporter.record_processed(100, None, None);
        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::FriJobManager, rx);

        reporter.record_processed(85, None, None);
        reporter.record_picked(85, None, None);
        monitor.evaluate_and_update_with_head(100, None);

        if let TransactionAcceptanceState::NotAccepting(reasons) = &*monitor.acceptance_tx.borrow()
        {
            assert_eq!(reasons.len(), 1);
            if let NotAcceptingReason::PipelineBackpressure { causes } = &reasons[0] {
                assert_eq!(causes.len(), 1);
                assert_eq!(causes[0].component, "fri_job_manager");
                assert!(matches!(
                    causes[0].trigger,
                    BackpressureTrigger::BlockDiffToUpstreamTooHigh {
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
    fn mid_pipeline_time_diff_to_upstream_does_not_cascade_to_downstream() {
        // Executor ts=2000. Canonizer picked ts=1950 → adjacent diff 50s > threshold
        // 30s (triggers). Applier picked ts=1940 → adjacent diff from Canonizer = 10s
        // (no trigger). Applier's head-relative lag of 60s must not be counted.
        let config = BackpressureConfig {
            block_pipeline: crate::config::PipelineCondition {
                max_block_diff_to_upstream: None,
                max_time_diff_to_upstream: Some(Duration::from_secs(30)),
                max_batch_diff_to_upstream: None,
            },
            ..BackpressureConfig::default()
        };
        let mut monitor = BackpressureMonitor::make_test_monitor(config);

        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (canon_reporter, canon_rx) = ComponentStateReporter::new("block_canonizer");
        let (apply_reporter, apply_rx) = ComponentStateReporter::new("block_applier");
        exec_reporter.record_processed(200, Some(2000), None);
        canon_reporter.record_processed(195, Some(1950), None);
        canon_reporter.record_picked(195, Some(1950), None);
        apply_reporter.record_processed(193, Some(1940), None);
        apply_reporter.record_picked(193, Some(1940), None);

        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::BlockCanonizer, canon_rx);
        monitor.register_linear(ComponentId::BlockApplier, apply_rx);

        monitor.evaluate_and_update_with_head(200, Some(2000));

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

    /// L1Sender and UpgradeGatekeeper never set `last_batch_picked`. With the batch-level
    /// fallback (`last_batch_picked → batch_number` in PipelineMaps) their `max_batch_diff_to_upstream`
    /// threshold still fires when upstream.batch_number outpaces their own batch_number.
    /// Regression guard — the fallback was once removed and silently disabled this
    /// threshold for those components.
    #[tokio::test]
    async fn l1_sender_style_component_triggers_max_batch_diff_to_upstream_via_fallback() {
        let config = batch_config_with_batch_diff_to_upstream(3);
        let mut monitor = BackpressureMonitor::make_test_monitor(config);
        let (exec_reporter, exec_rx) = ComponentStateReporter::new("block_executor");
        let (up_reporter, up_rx) = ComponentStateReporter::new("upgrade_gatekeeper");
        let (l1_reporter, l1_rx) = ComponentStateReporter::new("l1_sender_commit");
        exec_reporter.record_processed(100, None, None);
        up_reporter.record_processed(100, None, Some(10));
        // l1_sender deliberately does NOT call record_picked — relies on fallback.
        l1_reporter.record_processed(60, None, Some(5));

        monitor.register_linear(ComponentId::BlockExecutor, exec_rx);
        monitor.register_linear(ComponentId::UpgradeGatekeeper, up_rx);
        monitor.register_linear(ComponentId::L1SenderCommit, l1_rx);

        monitor.evaluate_and_update_with_head(100, None);

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
