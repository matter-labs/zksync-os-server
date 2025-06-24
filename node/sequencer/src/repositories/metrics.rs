use std::time::Duration;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
#[derive(Debug, Metrics)]
#[metrics(prefix = "repositories")]
pub struct RepositoriesMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub insert_block: LabeledFamily<&'static str, Histogram<Duration>>,
}

#[vise::register]
pub(crate) static REPOSITORIES_METRICS: vise::Global<RepositoriesMetrics> = vise::Global::new();
