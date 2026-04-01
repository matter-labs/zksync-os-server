use std::time::Duration;

/// Whether the node should be accepting transactions
#[derive(Debug, Clone, PartialEq)]
pub enum TransactionAcceptanceState {
    Accepting,
    NotAccepting(Vec<NotAcceptingReason>),
}

/// Reason why the node is not accepting transactions
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum NotAcceptingReason {
    /// Block production has been disabled via config (`sequencer_max_blocks_to_produce`)
    #[error("Node is not currently accepting transactions: block production disabled.")]
    BlockProductionDisabled,
    /// One or more pipeline components are reporting backpressure
    #[error("Node is not currently accepting transactions: pipeline backpressure ({} component(s) reporting).", causes.len())]
    PipelineBackpressure { causes: Vec<BackpressureCause> },
}

/// A single component contributing to pipeline backpressure
#[derive(Debug, Clone, PartialEq)]
pub struct BackpressureCause {
    pub component: &'static str,
    pub trigger: BackpressureTrigger,
}

/// The condition that triggered backpressure for a component
#[derive(Debug, Clone, PartialEq)]
pub enum BackpressureTrigger {
    /// The number of unprocessed blocks exceeds the threshold
    BlockLagTooHigh { threshold: u64, actual: u64 },
    /// The block-timestamp difference between head and this component exceeds the threshold.
    /// Only evaluated when both head and component timestamps are non-zero.
    TimeLagTooHigh {
        threshold: Duration,
        actual: Duration,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pipeline_backpressure_not_accepting() {
        let cause = BackpressureCause {
            component: "fri_job_manager",
            trigger: BackpressureTrigger::BlockLagTooHigh {
                threshold: 500,
                actual: 782,
            },
        };
        let state = TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::PipelineBackpressure {
                causes: vec![cause.clone()],
            },
        ]);
        assert!(matches!(state, TransactionAcceptanceState::NotAccepting(_)));
        assert_eq!(cause.component, "fri_job_manager");
    }

    #[test]
    fn time_lag_too_high_trigger() {
        let trigger = BackpressureTrigger::TimeLagTooHigh {
            threshold: Duration::from_secs(30),
            actual: Duration::from_secs(45),
        };
        assert!(matches!(
            trigger,
            BackpressureTrigger::TimeLagTooHigh { .. }
        ));
    }
}
