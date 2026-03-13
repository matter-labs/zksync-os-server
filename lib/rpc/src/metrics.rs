use std::time::Duration;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES: Buckets = Buckets::exponential(0.000001..=32.0, 2.0);
const BLOCK_COUNTS: Buckets = Buckets::exponential(1.0..=100000.0, 10.0);
const BYTES_BUCKETS: Buckets = Buckets::exponential(1.0..=10485760.0, 2.0); // 1B .. 10MB

#[derive(Debug, Metrics)]
pub struct ApiMetrics {
    #[metrics(labels = ["method"], buckets = BLOCK_COUNTS)]
    pub get_logs_scanned_blocks: LabeledFamily<&'static str, Histogram<u64>>,
    #[metrics(unit = Unit::Seconds, labels = ["method"], buckets = LATENCIES)]
    pub response_time: LabeledFamily<String, Histogram<Duration>>,
    #[metrics(unit = Unit::Bytes, labels = ["method"], buckets = BYTES_BUCKETS)]
    pub request_size: LabeledFamily<String, Histogram<usize>>,
    #[metrics(unit = Unit::Bytes, labels = ["method"], buckets = BYTES_BUCKETS)]
    pub response_size: LabeledFamily<String, Histogram<usize>>,
    #[metrics(labels = ["method"], buckets = Buckets::exponential(1.0..=1_000.0, 2.0))]
    pub requests_in_batch_count: LabeledFamily<String, Histogram<u64>>,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "http")]
pub struct HttpMetrics {
    /// Full HTTP request-response time, including TLS, parsing, serialization.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES)]
    pub response_time: Histogram<Duration>,
}

#[vise::register]
pub static API_METRICS: vise::Global<ApiMetrics> = vise::Global::new();

#[vise::register]
pub static HTTP_METRICS: vise::Global<HttpMetrics> = vise::Global::new();
