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
    #[error(
        "Node is not currently accepting transactions: pipeline backpressure ({}).",
        format_backpressure_components(causes)
    )]
    PipelineBackpressure { causes: Vec<BackpressureCause> },
}

fn format_backpressure_components(causes: &[BackpressureCause]) -> String {
    let mut names: Vec<&str> = causes.iter().map(|c| c.component).collect();
    names.sort_unstable();
    names.dedup();
    names.join(", ")
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
    /// The number of unprocessed blocks between this component and its upstream exceeds the threshold
    BlockDiffToUpstreamTooHigh { threshold: u64, actual: u64 },
    /// The block-timestamp diff between this component and its upstream exceeds the threshold.
    /// Only evaluated when both upstream and component timestamps are available.
    TimeDiffToUpstreamTooHigh {
        threshold: Duration,
        actual: Duration,
    },
    /// The number of unprocessed batches between this component and its upstream exceeds the threshold
    BatchDiffToUpstreamTooHigh { threshold: u64, actual: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pipeline_backpressure_not_accepting() {
        let cause = BackpressureCause {
            component: "fri_job_manager",
            trigger: BackpressureTrigger::BlockDiffToUpstreamTooHigh {
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
    fn time_diff_to_upstream_too_high_trigger() {
        let trigger = BackpressureTrigger::TimeDiffToUpstreamTooHigh {
            threshold: Duration::from_secs(30),
            actual: Duration::from_secs(45),
        };
        assert!(matches!(
            trigger,
            BackpressureTrigger::TimeDiffToUpstreamTooHigh { .. }
        ));
    }
}
