use std::time::Duration;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);

#[derive(Debug, Metrics)]
pub struct ApiMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["method"], buckets = LATENCIES_FAST)]
    pub response_time: LabeledFamily<&'static str, Histogram<Duration>>,
}
#[vise::register]
pub(crate) static API_METRICS: vise::Global<ApiMetrics> = vise::Global::new();
