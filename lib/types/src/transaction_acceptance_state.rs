use std::time::Duration;

/// Whether the node should be accepting transactions
#[derive(Debug, Clone, PartialEq)]
pub enum TransactionAcceptanceState {
    Accepting,
    NotAccepting(NotAcceptingReason),
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
    /// A downstream send has been blocked for too long
    WaitingSendTooLong {
        threshold: Duration,
        actual: Duration,
    },
    /// The number of unprocessed blocks exceeds the threshold
    BlockLagTooHigh { threshold: u64, actual: u64 },
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
        let state =
            TransactionAcceptanceState::NotAccepting(NotAcceptingReason::PipelineBackpressure {
                causes: vec![cause.clone()],
            });
        assert!(matches!(
            state,
            TransactionAcceptanceState::NotAccepting(
                NotAcceptingReason::PipelineBackpressure { .. }
            )
        ));
        assert_eq!(cause.component, "fri_job_manager");
    }

    #[test]
    fn waiting_send_too_long_trigger() {
        let trigger = BackpressureTrigger::WaitingSendTooLong {
            threshold: Duration::from_secs(3600),
            actual: Duration::from_secs(4215),
        };
        assert!(matches!(
            trigger,
            BackpressureTrigger::WaitingSendTooLong { .. }
        ));
    }
}
