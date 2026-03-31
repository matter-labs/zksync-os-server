use std::time::{SystemTime, UNIX_EPOCH};

use vise::{EncodeLabelValue, Gauge, LabeledFamily, Metrics, Unit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "outcome", rename_all = "snake_case")]
pub enum RevmDivergenceOutcome {
    Accepted,
    Reverted,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "revm_consistency_checker")]
pub struct RevmConsistencyCheckerMetrics {
    /// Unix timestamp of the most recent divergence by outcome -- used for alerts.
    #[metrics(unit = Unit::Seconds, labels = ["outcome"])]
    pub last_divergence_timestamp: LabeledFamily<RevmDivergenceOutcome, Gauge<u64>>,
}

#[vise::register]
pub static METRICS: vise::Global<RevmConsistencyCheckerMetrics> = vise::Global::new();

impl RevmConsistencyCheckerMetrics {
    pub fn record_divergence(&self, outcome: RevmDivergenceOutcome) {
        self.last_divergence_timestamp[&outcome].set(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
    }
}
