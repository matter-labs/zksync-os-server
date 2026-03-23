use crate::config::{ComponentId, PipelineHealthConfig};
use crate::metrics::{ComponentLabel, DirectionLabel, MONITOR_METRICS};
use futures::stream::{select_all, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::WatchStream;
use zksync_os_observability::{ComponentHealth, GenericComponentState};
use zksync_os_types::{
    BackpressureCause, BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState,
};

pub struct PipelineHealthMonitor {
    config: PipelineHealthConfig,
    components: Vec<(ComponentId, watch::Receiver<ComponentHealth>)>,
    queue_depths: Vec<(ComponentId, Arc<AtomicUsize>)>,
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
                queue_depths: vec![],
                acceptance_tx,
                stop_receiver,
            },
            acceptance_rx,
        )
    }

    pub fn register(&mut self, id: ComponentId, receiver: watch::Receiver<ComponentHealth>) {
        self.components.push((id, receiver));
    }

    pub fn register_queue_depth(&mut self, id: ComponentId, depth: Arc<AtomicUsize>) {
        self.queue_depths.push((id, depth));
    }

    pub async fn run(mut self) {
        // Prometheus metrics timer — independent of health evaluation.
        let mut metrics_tick = tokio::time::interval(self.config.metrics_interval);
        metrics_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        if self.components.is_empty() {
            // No components registered — just wait for stop.
            // NOTE: If a component stalls entirely (stops calling record_processed), the monitor
            // will only be woken by OTHER components' updates. The frozen component's lag will be
            // detected at the next wake-up. In a fully idle pipeline with no other components
            // active, the metrics_tick provides the safety net. A future improvement is heartbeat
            // updates from components. See: Option C decision in design discussion.
            let _ = self.stop_receiver.changed().await;
            return;
        }

        // Build a merged stream of all component health changes.
        // WatchStream::from_changes only yields on actual changes (not initial values).
        let streams = self
            .components
            .iter()
            .map(|(_, rx)| WatchStream::from_changes(rx.clone()))
            .collect::<Vec<_>>();
        let mut combined = select_all(streams);

        loop {
            tokio::select! {
                Some(_) = combined.next() => self.evaluate_and_update(),
                _ = metrics_tick.tick() => self.emit_metrics(),
                _ = self.stop_receiver.changed() => {
                    tracing::info!("PipelineHealthMonitor: stop signal received");
                    return;
                }
            }
        }
    }

    fn head_state(&self) -> (u64, u64) {
        self.components
            .iter()
            .find(|(id, _)| *id == ComponentId::BlockExecutor)
            .map(|(_, rx)| {
                let h = rx.borrow();
                (h.last_processed_seq, h.last_processed_block_timestamp)
            })
            .unwrap_or((0, 0))
    }

    fn evaluate_and_update(&self) {
        let (head_seq, head_ts) = self.head_state();
        self.evaluate_and_update_with_head(head_seq, head_ts);
    }

    pub(crate) fn evaluate_and_update_with_head(&self, head_seq: u64, head_ts: u64) {
        let mut active_causes: Vec<BackpressureCause> = self
            .components
            .iter()
            .filter_map(|(id, rx)| self.evaluate(*id, &rx.borrow(), head_seq, head_ts))
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
            match &new_state {
                TransactionAcceptanceState::NotAccepting(reason) => {
                    tracing::warn!(
                        ?reason,
                        "pipeline backpressure: stopping transaction acceptance"
                    );
                    MONITOR_METRICS.acceptance_state_changes[&DirectionLabel {
                        direction: "open",
                    }]
                    .inc();
                }
                TransactionAcceptanceState::Accepting => {
                    tracing::info!(
                        "pipeline backpressure cleared: resuming transaction acceptance"
                    );
                    MONITOR_METRICS.acceptance_state_changes[&DirectionLabel {
                        direction: "cleared",
                    }]
                    .inc();
                }
            }
            *current = new_state.clone();
            true
        });
    }

    pub(crate) fn evaluate(
        &self,
        id: ComponentId,
        health: &ComponentHealth,
        head_seq: u64,
        head_ts: u64,
    ) -> Option<BackpressureCause> {
        let condition = self.config.condition_for(id);

        if let Some(max_lag) = condition.max_block_lag {
            let lag = head_seq.saturating_sub(health.last_processed_seq);
            if lag > max_lag {
                return Some(BackpressureCause {
                    component: id.as_str(),
                    trigger: BackpressureTrigger::BlockLagTooHigh {
                        threshold: max_lag,
                        actual: lag,
                    },
                });
            }
        }

        if let Some(max_time_lag) = condition.max_time_lag {
            let comp_ts = health.last_processed_block_timestamp;
            // Only evaluate if both timestamps are available (non-zero).
            if comp_ts > 0 && head_ts > 0 {
                let lag_secs = head_ts.saturating_sub(comp_ts);
                let actual = Duration::from_secs(lag_secs);
                if actual > max_time_lag {
                    return Some(BackpressureCause {
                        component: id.as_str(),
                        trigger: BackpressureTrigger::TimeLagTooHigh {
                            threshold: max_time_lag,
                            actual,
                        },
                    });
                }
            }
        }

        None
    }

    pub(crate) fn emit_metrics(&self) {
        let (head_seq, head_ts) = self.head_state();

        // Recompute active causes for metric labelling.
        let active_components: std::collections::HashSet<&'static str> = self
            .components
            .iter()
            .filter_map(|(id, rx)| self.evaluate(*id, &rx.borrow(), head_seq, head_ts))
            .map(|c| c.component)
            .collect();

        for (id, rx) in &self.components {
            let health = rx.borrow();
            let label = ComponentLabel::from(*id);

            MONITOR_METRICS.backpressure_active[&label]
                .set(active_components.contains(id.as_str()) as u64);
            MONITOR_METRICS.component_last_processed_block[&label]
                .set(health.last_processed_seq);
            MONITOR_METRICS.component_block_lag[&label]
                .set(head_seq.saturating_sub(health.last_processed_seq));

            let time_lag =
                if health.last_processed_block_timestamp > 0 && head_ts > 0 {
                    head_ts.saturating_sub(health.last_processed_block_timestamp) as f64
                } else {
                    0.0
                };
            MONITOR_METRICS.component_time_lag_seconds[&label].set(time_lag);
        }

        for (id, depth) in &self.queue_depths {
            let label = ComponentLabel::from(*id);
            MONITOR_METRICS.channel_queue_depth[&label]
                .set(depth.load(Ordering::Relaxed) as u64);
        }
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
    use crate::config::{BackpressureCondition, ComponentId, PipelineHealthConfig};
    use std::time::Duration;
    use tokio::time::Instant;
    use zksync_os_observability::GenericComponentState;
    use zksync_os_types::BackpressureTrigger;

    fn make_health(seq: u64, ts: u64) -> ComponentHealth {
        ComponentHealth {
            state: GenericComponentState::Processing,
            state_entered_at: Instant::now(),
            last_processed_seq: seq,
            last_processed_block_timestamp: ts,
        }
    }

    fn config_with_block_lag(id: ComponentId, max_lag: u64) -> PipelineHealthConfig {
        let mut config = PipelineHealthConfig::default();
        let cond = BackpressureCondition {
            max_block_lag: Some(max_lag),
            max_time_lag: None,
        };
        match id {
            ComponentId::BlockApplier => config.block_applier = cond,
            ComponentId::BlockExecutor => config.block_executor = cond,
            ComponentId::FriJobManager => config.fri_job_manager = cond,
            _ => {}
        }
        config
    }

    fn config_with_time_lag(id: ComponentId, max_lag: Duration) -> PipelineHealthConfig {
        let mut config = PipelineHealthConfig::default();
        let cond = BackpressureCondition {
            max_block_lag: None,
            max_time_lag: Some(max_lag),
        };
        match id {
            ComponentId::BlockApplier => config.block_applier = cond,
            _ => {}
        }
        config
    }

    #[test]
    fn below_lag_threshold_no_trigger() {
        let config = config_with_block_lag(ComponentId::BlockApplier, 10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head=100, applier=95, lag=5 < 10
        let result = monitor.evaluate(ComponentId::BlockApplier, &make_health(95, 0), 100, 0);
        assert!(result.is_none());
    }

    #[test]
    fn above_lag_threshold_triggers() {
        let config = config_with_block_lag(ComponentId::BlockApplier, 10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head=100, applier=85, lag=15 > 10
        let result = monitor.evaluate(ComponentId::BlockApplier, &make_health(85, 0), 100, 0);
        assert!(matches!(
            result.map(|c| c.trigger),
            Some(BackpressureTrigger::BlockLagTooHigh {
                threshold: 10,
                actual: 15
            })
        ));
    }

    #[test]
    fn at_exact_threshold_no_trigger() {
        let config = config_with_block_lag(ComponentId::BlockApplier, 10);
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // lag == threshold: should NOT trigger (strictly greater than)
        let result = monitor.evaluate(ComponentId::BlockApplier, &make_health(90, 0), 100, 0);
        assert!(result.is_none());
    }

    #[test]
    fn time_lag_triggers_when_exceeded() {
        let config = config_with_time_lag(ComponentId::BlockApplier, Duration::from_secs(30));
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head_ts=1000, applier_ts=960, lag=40s > 30s
        let result = monitor.evaluate(ComponentId::BlockApplier, &make_health(90, 960), 100, 1000);
        assert!(matches!(
            result.map(|c| c.trigger),
            Some(BackpressureTrigger::TimeLagTooHigh { .. })
        ));
    }

    #[test]
    fn time_lag_skipped_when_component_timestamp_zero() {
        let config = config_with_time_lag(ComponentId::BlockApplier, Duration::from_secs(1));
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // component timestamp = 0 → unavailable, must not trigger
        let result = monitor.evaluate(ComponentId::BlockApplier, &make_health(90, 0), 100, 1000);
        assert!(result.is_none());
    }

    #[test]
    fn time_lag_skipped_when_head_timestamp_zero() {
        let config = config_with_time_lag(ComponentId::BlockApplier, Duration::from_secs(1));
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        // head timestamp = 0 → unavailable, must not trigger
        let result = monitor.evaluate(ComponentId::BlockApplier, &make_health(90, 900), 100, 0);
        assert!(result.is_none());
    }

    #[test]
    fn no_condition_set_never_triggers() {
        let config = PipelineHealthConfig::default();
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        let result =
            monitor.evaluate(ComponentId::BlockApplier, &make_health(0, 0), 10_000, 999_999);
        assert!(result.is_none());
    }

    #[test]
    fn evaluate_and_update_sets_accepting_when_no_causes() {
        let config = PipelineHealthConfig::default();
        let monitor = PipelineHealthMonitor::make_test_monitor(config);
        monitor.evaluate_and_update_with_head(100, 0);
        assert_eq!(
            *monitor.acceptance_tx.borrow(),
            TransactionAcceptanceState::Accepting
        );
    }
}
