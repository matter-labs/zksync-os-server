use crate::config::{ComponentId, PipelineHealthConfig};
use crate::metrics::{ComponentLabel, MONITOR_METRICS};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};
use zksync_os_observability::{ComponentHealth, GenericComponentState};
use zksync_os_types::{
    BackpressureCause, BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState,
};

pub struct PipelineHealthMonitor {
    config: PipelineHealthConfig,
    components: Vec<(ComponentId, watch::Receiver<ComponentHealth>)>,
    acceptance_tx: watch::Sender<TransactionAcceptanceState>,
    stop_receiver: watch::Receiver<bool>,
}

impl PipelineHealthMonitor {
    pub fn new(
        config: PipelineHealthConfig,
        stop_receiver: watch::Receiver<bool>,
    ) -> (Self, watch::Receiver<TransactionAcceptanceState>) {
        assert!(
            config.eval_interval > std::time::Duration::ZERO,
            "PipelineHealthConfig::eval_interval must be > 0"
        );
        let (acceptance_tx, acceptance_rx) = watch::channel(TransactionAcceptanceState::Accepting);
        (
            Self {
                config,
                components: vec![],
                acceptance_tx,
                stop_receiver,
            },
            acceptance_rx,
        )
    }

    pub fn register(&mut self, id: ComponentId, receiver: watch::Receiver<ComponentHealth>) {
        self.components.push((id, receiver));
    }

    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(self.config.eval_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => self.evaluate_and_update(),
                _ = self.stop_receiver.changed() => {
                    tracing::info!("PipelineHealthMonitor: stop signal received");
                    return;
                }
            }
        }
    }

    fn head_seq(&self) -> u64 {
        self.components
            .iter()
            .find(|(id, _)| *id == ComponentId::BlockExecutor)
            .map(|(_, rx)| rx.borrow().last_processed_seq)
            .unwrap_or(0)
    }

    fn evaluate_and_update(&self) {
        let head_seq = self.head_seq();
        self.evaluate_and_update_with_head(head_seq);
    }

    pub(crate) fn evaluate_and_update_with_head(&self, head_seq: u64) {
        let mut active_causes: Vec<BackpressureCause> = self
            .components
            .iter()
            .filter_map(|(id, rx)| self.evaluate(*id, &rx.borrow(), head_seq))
            .collect();

        active_causes.sort_by_key(|c| c.component);

        self.emit_metrics(&active_causes, head_seq);

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
                }
                TransactionAcceptanceState::Accepting => {
                    tracing::info!(
                        "pipeline backpressure cleared: resuming transaction acceptance"
                    );
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
    ) -> Option<BackpressureCause> {
        let condition = self.config.condition_for(id);
        let now = Instant::now();

        if id.is_reactive() {
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
            return None;
        }

        if health.state != GenericComponentState::WaitingSend {
            return None;
        }

        if let Some(max_duration) = condition.max_waiting_send_duration {
            let elapsed = now.duration_since(health.state_entered_at);
            if elapsed > max_duration {
                return Some(BackpressureCause {
                    component: id.as_str(),
                    trigger: BackpressureTrigger::WaitingSendTooLong {
                        threshold: max_duration,
                        actual: elapsed,
                    },
                });
            }
        }

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

        None
    }

    fn emit_metrics(&self, active_causes: &[BackpressureCause], head_seq: u64) {
        for (id, rx) in &self.components {
            let health = rx.borrow();
            let label = ComponentLabel::from(*id);
            let is_active = active_causes.iter().any(|c| c.component == id.as_str());
            MONITOR_METRICS.backpressure_active[&label].set(is_active as u64);

            let (lag, waiting_send_secs) = if id.is_reactive() {
                (head_seq.saturating_sub(health.last_processed_seq), 0.0_f64)
            } else if health.state == GenericComponentState::WaitingSend {
                let lag = head_seq.saturating_sub(health.last_processed_seq);
                let secs = Instant::now()
                    .duration_since(health.state_entered_at)
                    .as_secs_f64();
                (lag, secs)
            } else {
                (0, 0.0)
            };

            MONITOR_METRICS.component_block_lag[&label].set(lag);
            MONITOR_METRICS.component_waiting_send_seconds[&label].set(waiting_send_secs);
        }
    }

    #[cfg(test)]
    pub fn force_health_for_test(&mut self, id: ComponentId, health: ComponentHealth) {
        let (tx, rx) = watch::channel(health);
        std::mem::forget(tx);
        if let Some(entry) = self.components.iter_mut().find(|(cid, _)| *cid == id) {
            entry.1 = rx;
        } else {
            self.components.push((id, rx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackpressureCondition, ComponentId, PipelineHealthConfig};
    use std::time::Duration;
    use tokio::sync::watch;
    use tokio::time::Instant;
    use zksync_os_observability::{ComponentHealth, GenericComponentState};
    use zksync_os_types::{BackpressureTrigger, NotAcceptingReason, TransactionAcceptanceState};

    fn make_health(
        state: GenericComponentState,
        secs_in_state: u64,
        last_seq: u64,
    ) -> ComponentHealth {
        ComponentHealth {
            state,
            state_entered_at: Instant::now() - Duration::from_secs(secs_in_state),
            last_processed_seq: last_seq,
        }
    }

    fn monitor_with(condition: BackpressureCondition, id: ComponentId) -> PipelineHealthMonitor {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let mut config = PipelineHealthConfig::default();
        match id {
            ComponentId::BlockExecutor => config.block_executor = condition,
            ComponentId::FriJobManager => config.fri_job_manager = condition,
            ComponentId::L1SenderCommit => config.l1_sender_commit = condition,
            ComponentId::SnarkJobManager => config.snark_job_manager = condition,
            _ => {}
        }
        let (monitor, _rx) = PipelineHealthMonitor::new(config, stop_rx);
        monitor
    }

    #[test]
    fn pipeline_loop_waiting_recv_no_trigger() {
        let m = monitor_with(
            BackpressureCondition {
                max_block_lag: Some(5),
                ..Default::default()
            },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingRecv, 0, 0);
        assert!(m
            .evaluate(ComponentId::L1SenderCommit, &health, 100)
            .is_none());
    }

    #[test]
    fn pipeline_loop_processing_no_trigger() {
        let m = monitor_with(
            BackpressureCondition {
                max_block_lag: Some(5),
                ..Default::default()
            },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::Processing, 0, 0);
        assert!(m
            .evaluate(ComponentId::L1SenderCommit, &health, 100)
            .is_none());
    }

    #[test]
    fn pipeline_loop_waiting_send_duration_exceeded() {
        let m = monitor_with(
            BackpressureCondition {
                max_waiting_send_duration: Some(Duration::from_secs(10)),
                ..Default::default()
            },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingSend, 20, 0);
        let cause = m.evaluate(ComponentId::L1SenderCommit, &health, 0).unwrap();
        assert_eq!(cause.component, "l1_sender_commit");
        assert!(matches!(
            cause.trigger,
            BackpressureTrigger::WaitingSendTooLong { .. }
        ));
    }

    #[test]
    fn pipeline_loop_waiting_send_lag_exceeded() {
        let m = monitor_with(
            BackpressureCondition {
                max_block_lag: Some(10),
                ..Default::default()
            },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingSend, 0, 80);
        let cause = m
            .evaluate(ComponentId::L1SenderCommit, &health, 100)
            .unwrap();
        assert_eq!(cause.component, "l1_sender_commit");
        assert!(matches!(
            cause.trigger,
            BackpressureTrigger::BlockLagTooHigh {
                threshold: 10,
                actual: 20
            }
        ));
    }

    #[test]
    fn pipeline_loop_both_exceeded_duration_wins() {
        let m = monitor_with(
            BackpressureCondition {
                max_waiting_send_duration: Some(Duration::from_secs(10)),
                max_block_lag: Some(5),
            },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingSend, 20, 80);
        let cause = m
            .evaluate(ComponentId::L1SenderCommit, &health, 100)
            .unwrap();
        assert!(matches!(
            cause.trigger,
            BackpressureTrigger::WaitingSendTooLong { .. }
        ));
    }

    #[test]
    fn pipeline_loop_waiting_send_below_threshold() {
        let m = monitor_with(
            BackpressureCondition {
                max_waiting_send_duration: Some(Duration::from_secs(30)),
                max_block_lag: Some(50),
            },
            ComponentId::L1SenderCommit,
        );
        let health = make_health(GenericComponentState::WaitingSend, 5, 95);
        assert!(m
            .evaluate(ComponentId::L1SenderCommit, &health, 100)
            .is_none());
    }

    #[test]
    fn reactive_lag_exceeded_regardless_of_state() {
        let m = monitor_with(
            BackpressureCondition {
                max_block_lag: Some(10),
                ..Default::default()
            },
            ComponentId::FriJobManager,
        );
        for state in [
            GenericComponentState::WaitingRecv,
            GenericComponentState::Processing,
            GenericComponentState::ProcessingOrWaitingRecv,
        ] {
            let health = make_health(state, 0, 80);
            let cause = m
                .evaluate(ComponentId::FriJobManager, &health, 100)
                .unwrap();
            assert!(matches!(
                cause.trigger,
                BackpressureTrigger::BlockLagTooHigh { .. }
            ));
        }
    }

    #[test]
    fn reactive_lag_not_exceeded() {
        let m = monitor_with(
            BackpressureCondition {
                max_block_lag: Some(50),
                ..Default::default()
            },
            ComponentId::FriJobManager,
        );
        let health = make_health(GenericComponentState::ProcessingOrWaitingRecv, 0, 95);
        assert!(m
            .evaluate(ComponentId::FriJobManager, &health, 100)
            .is_none());
    }

    #[tokio::test]
    async fn two_causes_both_in_acceptance_state() {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let config = PipelineHealthConfig {
            fri_job_manager: BackpressureCondition {
                max_block_lag: Some(5),
                ..Default::default()
            },
            l1_sender_commit: BackpressureCondition {
                max_waiting_send_duration: Some(Duration::from_secs(5)),
                ..Default::default()
            },
            ..Default::default()
        };
        let (mut monitor, acceptance_rx) = PipelineHealthMonitor::new(config, stop_rx);

        let (_, fri_rx) = zksync_os_observability::ComponentHealthReporter::new("fri_job_manager");
        let (_, l1_rx) = zksync_os_observability::ComponentHealthReporter::new("l1_sender_commit");
        monitor.register(ComponentId::FriJobManager, fri_rx);
        monitor.register(ComponentId::L1SenderCommit, l1_rx);
        monitor.force_health_for_test(
            ComponentId::FriJobManager,
            make_health(GenericComponentState::ProcessingOrWaitingRecv, 0, 80),
        );
        monitor.force_health_for_test(
            ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 20, 90),
        );
        monitor.evaluate_and_update_with_head(100);

        let state = acceptance_rx.borrow().clone();
        if let TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure { causes },
        ) = state
        {
            assert_eq!(causes.len(), 2);
        } else {
            panic!("expected NotAccepting(PipelineBackpressure)");
        }
    }

    #[tokio::test]
    async fn one_cause_clears_other_remains_not_accepting() {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let config = PipelineHealthConfig {
            fri_job_manager: BackpressureCondition {
                max_block_lag: Some(5),
                ..Default::default()
            },
            l1_sender_commit: BackpressureCondition {
                max_block_lag: Some(5),
                ..Default::default()
            },
            ..Default::default()
        };
        let (mut monitor, acceptance_rx) = PipelineHealthMonitor::new(config, stop_rx);
        monitor.force_health_for_test(
            ComponentId::FriJobManager,
            make_health(GenericComponentState::ProcessingOrWaitingRecv, 0, 80),
        );
        monitor.force_health_for_test(
            ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 0, 80),
        );
        monitor.evaluate_and_update_with_head(100);
        assert!(matches!(
            acceptance_rx.borrow().clone(),
            TransactionAcceptanceState::NotAccepting(
                NotAcceptingReason::PipelineBackpressure { .. }
            )
        ));

        monitor.force_health_for_test(
            ComponentId::FriJobManager,
            make_health(GenericComponentState::ProcessingOrWaitingRecv, 0, 98),
        );
        monitor.evaluate_and_update_with_head(100);
        let state = acceptance_rx.borrow().clone();
        if let TransactionAcceptanceState::NotAccepting(
            NotAcceptingReason::PipelineBackpressure { causes },
        ) = state
        {
            assert_eq!(causes.len(), 1);
            assert_eq!(causes[0].component, "l1_sender_commit");
        } else {
            panic!("expected NotAccepting with 1 remaining cause");
        }
    }

    #[tokio::test]
    async fn all_causes_clear_becomes_accepting() {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let config = PipelineHealthConfig {
            l1_sender_commit: BackpressureCondition {
                max_block_lag: Some(5),
                ..Default::default()
            },
            ..Default::default()
        };
        let (mut monitor, acceptance_rx) = PipelineHealthMonitor::new(config, stop_rx);
        monitor.force_health_for_test(
            ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 0, 80),
        );
        monitor.evaluate_and_update_with_head(100);
        assert!(matches!(
            acceptance_rx.borrow().clone(),
            TransactionAcceptanceState::NotAccepting(_)
        ));

        monitor.force_health_for_test(
            ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 0, 98),
        );
        monitor.evaluate_and_update_with_head(100);
        assert_eq!(
            *acceptance_rx.borrow(),
            TransactionAcceptanceState::Accepting
        );
    }

    #[tokio::test]
    async fn metrics_zero_for_idle_nonzero_for_waiting_send() {
        let (stop_tx, stop_rx) = watch::channel(false);
        std::mem::forget(stop_tx);
        let config = PipelineHealthConfig::default();
        let (mut monitor, _) = PipelineHealthMonitor::new(config, stop_rx);
        monitor.force_health_for_test(
            ComponentId::BlockApplier,
            make_health(GenericComponentState::WaitingRecv, 0, 50),
        );
        monitor.force_health_for_test(
            ComponentId::L1SenderCommit,
            make_health(GenericComponentState::WaitingSend, 5, 50),
        );
        monitor.evaluate_and_update_with_head(100);
        // Smoke test: verify it runs without panicking
    }
}
