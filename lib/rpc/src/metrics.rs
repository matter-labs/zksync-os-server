use std::time::Duration;
use alloy::rpc::types::Filter;
use vise::{Buckets, Counter, EncodeLabelValue, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.000001..=32.0, 2.0);
const BLOCK_COUNTS: Buckets = Buckets::exponential(1.0..=100000.0, 10.0);
const BYTES_BUCKETS: Buckets = Buckets::exponential(1.0..=10485760.0, 2.0); // 1B .. 10MB
const RATIO_BUCKETS: Buckets =
    Buckets::values(&[0.0, 0.01, 0.05, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.99, 1.0]);

/// Dimension for per-call `eth_getLogs` scan statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "kind", rename_all = "snake_case")]
pub enum GetLogsStat {
    Total,
    SkippedByIndex,
    BloomTruePositive,
    BloomFalsePositive,
    BloomNegative,
    LogsReturned,
}

/// Filter constraint category for an `eth_getLogs` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "filter", rename_all = "snake_case")]
pub enum FilterCategory {
    /// No address or topic constraints; every block must be scanned.
    Unconstrained,
    /// Address constraint only; index can skip by address.
    AddressOnly,
    /// Topic constraint(s) only; index can skip by topic.
    TopicOnly,
    /// Both address and topic constraints; index uses both.
    AddressAndTopic,
}

impl From<&Filter> for FilterCategory {
    fn from(filter: &Filter) -> Self {
        match (filter.address.is_empty(), filter.has_topics()) {
            (true, false) => Self::Unconstrained,
            (false, false) => Self::AddressOnly,
            (true, true) => Self::TopicOnly,
            (false, true) => Self::AddressAndTopic,
        }
    }
}

#[derive(Debug, Metrics)]
pub struct ApiMetrics {
    /// Block disposition per `eth_getLogs` call, broken down by outcome kind and filter category.
    #[metrics(labels = ["kind", "filter"], buckets = BLOCK_COUNTS)]
    pub get_logs_scanned_blocks: LabeledFamily<(GetLogsStat, FilterCategory), Histogram<u64>, 2>,
    /// Per-call fraction of blocks skipped by the log index (skipped / total).
    #[metrics(labels = ["filter"], buckets = RATIO_BUCKETS)]
    pub get_logs_index_skip_ratio: LabeledFamily<FilterCategory, Histogram<f64>>,
    /// Per-call bloom filter false-positive rate among blocks that reached the bloom check.
    #[metrics(labels = ["filter"], buckets = RATIO_BUCKETS)]
    pub get_logs_bloom_fp_rate: LabeledFamily<FilterCategory, Histogram<f64>>,
    /// Per-call fraction of the queried block range covered by the log index.
    #[metrics(labels = ["filter"], buckets = RATIO_BUCKETS)]
    pub get_logs_index_coverage: LabeledFamily<FilterCategory, Histogram<f64>>,
    /// Number of `eth_getLogs` calls truncated due to exceeding `max_logs`.
    pub get_logs_truncated: Counter,
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
