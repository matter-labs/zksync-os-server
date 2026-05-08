use super::ProviderKind;
use std::time::Duration;
use vise::{Buckets, Counter, Histogram, LabeledFamily, Metrics, MetricsFamily, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.000001..=32.0, 2.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "provider")]
pub(super) struct ProviderMetrics {
    /// This is end to end so retries & backoff time is included
    #[metrics(unit = Unit::Seconds, labels = ["method"], buckets = LATENCIES_FAST)]
    pub response_time: LabeledFamily<String, Histogram<Duration>>,
    pub retry_count: Counter,
}

#[vise::register]
pub(super) static METRICS: MetricsFamily<ProviderKind, ProviderMetrics> = MetricsFamily::new();
