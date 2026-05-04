use alloy::rpc::types::Filter;
use std::time::Duration;
use vise::{
    Buckets, Counter, EncodeLabelSet, EncodeLabelValue, Family, Histogram, LabeledFamily, Metrics,
    Unit,
};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.000001..=32.0, 2.0);
const BLOCK_COUNTS: Buckets = Buckets::exponential(1.0..=100000.0, 10.0);
const BYTES_BUCKETS: Buckets = Buckets::exponential(1.0..=10485760.0, 2.0); // 1B .. 10MB
const RATIO_BUCKETS: Buckets = Buckets::values(&[
    0.0, 0.01, 0.05, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.99, 1.0,
]);

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue, EncodeLabelSet)]
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

/// Metrics for `eth_getLogs` scan behaviour.
#[derive(Debug, Metrics)]
#[metrics(prefix = "get_logs")]
pub struct GetLogs {
    /// Block disposition per call, broken down by outcome kind and filter category.
    #[metrics(labels = ["kind", "filter"], buckets = BLOCK_COUNTS)]
    pub scanned_blocks: LabeledFamily<(GetLogsStat, FilterCategory), Histogram<u64>, 2>,
    /// Per-call fraction of blocks skipped by the log index (skipped / total).
    #[metrics(buckets = RATIO_BUCKETS)]
    pub index_skip_ratio: Family<FilterCategory, Histogram<f64>>,
    /// Per-call bloom filter precision among blocks that reached the bloom check (true positives / bloom-passed blocks).
    #[metrics(buckets = RATIO_BUCKETS)]
    pub bloom_precision: Family<FilterCategory, Histogram<f64>>,
    /// Per-call fraction of the queried block range covered by the log index.
    #[metrics(buckets = RATIO_BUCKETS)]
    pub index_coverage: Family<FilterCategory, Histogram<f64>>,
    /// Number of calls truncated due to exceeding `max_logs`, broken down by filter category.
    pub truncated: Family<FilterCategory, Counter>,
}

#[vise::register]
pub static GET_LOGS: vise::Global<GetLogs> = vise::Global::new();

#[derive(Debug, Metrics)]
#[metrics(prefix = "api")]
pub struct Api {
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
    #[metrics(labels = ["method"])]
    pub panicked: LabeledFamily<String, Counter>,
}

#[vise::register]
pub static API_METRICS: vise::Global<Api> = vise::Global::new();

/// Metrics for the transaction submission pipeline.
#[derive(Debug, Metrics)]
#[metrics(prefix = "tx_submission")]
pub struct TxSubmission {
    /// Time spent validating and inserting a transaction into the local mempool.
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub mempool_latency: Histogram<Duration>,
    /// Time spent forwarding a transaction to the main node (external nodes only).
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub forwarding_latency: Histogram<Duration>,
    /// Count of `eth_sendRawTransaction` rejections, labeled by reason.
    pub rejections: Family<TxRejectionReason, Counter>,
}

#[vise::register]
pub static TX_SUBMISSION: vise::Global<TxSubmission> = vise::Global::new();

/// Reason why an `eth_sendRawTransaction` was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue, EncodeLabelSet)]
#[metrics(label = "reason", rename_all = "snake_case")]
pub enum TxRejectionReason {
    /// Client sent bytes that don't decode as a valid signed transaction.
    DecodeFailed,
    /// Transaction signature is invalid.
    InvalidSignature,
    /// Node is temporarily not accepting transactions (e.g. syncing).
    NotAccepting,
    /// The signer address is on the blacklist.
    BlacklistedSigner,
    /// Forwarding the transaction to the main node failed (transport error).
    ForwardTransportError,
    /// Forwarding the transaction to the main node failed (main node rejected it).
    ForwardRejected,
    /// Transaction already exists in the pool.
    PoolAlreadyImported,
    /// Replacement transaction's gas price is too low.
    PoolReplacementUnderpriced,
    /// Transaction fee cap is below the protocol minimum.
    PoolFeeCapBelowMinimum,
    /// Sender exceeded the per-account slot capacity.
    PoolSpammerExceededCapacity,
    /// Transaction discarded due to pool size limits.
    PoolDiscardedOnInsert,
    /// Transaction type conflicts with an existing transaction for this sender.
    PoolConflictingTxType,
    /// Transaction exceeds the block gas limit.
    PoolExceedsGasLimit,
    /// Transaction gas limit exceeds per-tx maximum.
    PoolMaxTxGasLimitExceeded,
    /// Transaction fee exceeds the configured cap.
    PoolExceedsFeeCap,
    /// Transaction init code exceeds max size.
    PoolExceedsMaxInitCodeSize,
    /// Transaction input data is oversized.
    PoolOversizedData,
    /// Transaction is underpriced.
    PoolUnderpriced,
    /// Transaction would overdraw the sender's balance.
    PoolOverdraft,
    /// Nonce exceeds u64 limit (EIP-2681).
    PoolNonceOverflow,
    /// Intrinsic gas is too low.
    PoolIntrinsicGasTooLow,
    /// Transaction priority fee is below the minimum.
    PoolPriorityFeeBelowMinimum,
    /// Consensus-level invalid transaction.
    PoolConsensusError,
    /// EIP-4844 blob transaction error.
    PoolEip4844Error,
    /// EIP-7702 transaction error.
    PoolEip7702Error,
    /// Other pool error.
    PoolOther,
}
