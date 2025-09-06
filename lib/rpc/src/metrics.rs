use std::time::Duration;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
const BLOCK_COUNTS: Buckets = Buckets::exponential(1.0..=100000.0, 10.0);
const BYTES_BUCKETS: Buckets = Buckets::exponential(1.0..=10485760.0, 2.0); // 1B .. 10MB

#[derive(Debug, Metrics)]
pub struct ApiMetrics {
    #[metrics(labels = ["method"], buckets = BLOCK_COUNTS)]
    pub get_logs_scanned_blocks: LabeledFamily<&'static str, Histogram<u64>>,
    #[metrics(unit = Unit::Seconds, labels = ["method"], buckets = LATENCIES_FAST)]
    pub response_time: LabeledFamily<String, Histogram<Duration>>,
    #[metrics(unit = Unit::Bytes, labels = ["method"], buckets = BYTES_BUCKETS)]
    pub response_size: LabeledFamily<String, Histogram<usize>>,
}

#[vise::register]
pub static API_METRICS: vise::Global<ApiMetrics> = vise::Global::new();
