use std::time::Duration;
use vise::{Buckets, Counter, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.000001..=32.0, 2.0);
const BLOCK_COUNTS: Buckets = Buckets::exponential(1.0..=100000.0, 10.0);
const BYTES_BUCKETS: Buckets = Buckets::exponential(1.0..=10485760.0, 2.0); // 1B .. 10MB

#[derive(Debug, Metrics)]
pub struct ApiMetrics {
    #[metrics(labels = ["method"], buckets = BLOCK_COUNTS)]
    pub get_logs_scanned_blocks: LabeledFamily<&'static str, Histogram<u64>>,
    #[metrics(unit = Unit::Seconds, labels = ["method"], buckets = LATENCIES_FAST)]
    pub response_time: LabeledFamily<String, Histogram<Duration>>,
    #[metrics(unit = Unit::Bytes, labels = ["method"], buckets = BYTES_BUCKETS)]
    pub request_size: LabeledFamily<String, Histogram<usize>>,
    #[metrics(unit = Unit::Bytes, labels = ["method"], buckets = BYTES_BUCKETS)]
    pub response_size: LabeledFamily<String, Histogram<usize>>,
    #[metrics(labels = ["method"], buckets = Buckets::exponential(1.0..=1_000.0, 2.0))]
    pub requests_in_batch_count: LabeledFamily<String, Histogram<u64>>,
    #[metrics(labels = ["method", "code"])]
    pub errors: LabeledFamily<(String, i32), Counter, 2>,
    #[metrics(labels = ["method"])]
    pub cancelled: LabeledFamily<String, Counter>,
}

#[vise::register]
pub static API_METRICS: vise::Global<ApiMetrics> = vise::Global::new();

/// Metrics for the transaction submission pipeline.
#[derive(Debug, Metrics)]
#[metrics(prefix = "tx_submission")]
pub struct TxSubmissionMetrics {
    /// Time spent validating and inserting a transaction into the local mempool.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub mempool_latency: Histogram<Duration>,
    /// Time spent forwarding a transaction to the main node (external nodes only).
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub forwarding_latency: Histogram<Duration>,
}

#[vise::register]
pub static TX_SUBMISSION_METRICS: vise::Global<TxSubmissionMetrics> = vise::Global::new();
