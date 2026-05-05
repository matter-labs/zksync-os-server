use std::time::Duration;
use vise::{Buckets, Counter, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.000001..=32.0, 2.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "l1_provider")]
pub(super) struct L1ProviderMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["method"], buckets = LATENCIES_FAST)]
    pub response_time: LabeledFamily<String, Histogram<Duration>>,
    pub retry_count: Counter,
}

#[vise::register]
pub(super) static METRICS: vise::Global<L1ProviderMetrics> = vise::Global::new();
