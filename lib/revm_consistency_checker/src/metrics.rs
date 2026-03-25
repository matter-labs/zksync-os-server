use std::time::{SystemTime, UNIX_EPOCH};

use vise::{Gauge, Metrics};

#[derive(Debug, Metrics)]
#[metrics(prefix = "revm_consistency_checker")]
pub(crate) struct RevmConsistencyCheckerMetrics {
    /// Unix timestamp of the most recent detected inconsistency.
    pub last_inconsistency_timestamp: Gauge<u64>,
    /// Block number of the most recent detected inconsistency.
    pub last_inconsistent_block_number: Gauge<u64>,
}

#[vise::register]
pub(crate) static METRICS: vise::Global<RevmConsistencyCheckerMetrics> = vise::Global::new();

impl RevmConsistencyCheckerMetrics {
    pub fn record_inconsistency(&self, block_number: u64) {
        self.last_inconsistent_block_number.set(block_number);
        self.last_inconsistency_timestamp.set(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
    }
}
