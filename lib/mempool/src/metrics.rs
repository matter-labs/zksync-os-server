use metrics::{
    CounterFn, GaugeFn, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString, Unit,
};
use std::sync::Arc;
use vise::{Buckets, Counter, Gauge, Histogram, Metrics};

/// Mempool metrics.
///
/// This is a direct copy of [`reth_transaction_pool::metrics::TxPoolMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "transaction_pool")]
pub struct TxPoolMetrics {
    /// Number of transactions inserted in the pool
    pub(crate) inserted_transactions: Counter,
    /// Number of invalid transactions
    pub(crate) invalid_transactions: Counter,
    /// Number of removed transactions from the pool
    pub(crate) removed_transactions: Counter,

    /// Number of transactions in the pending sub-pool
    pub(crate) pending_pool_transactions: Gauge,
    /// Total amount of memory used by the transactions in the pending sub-pool in bytes
    pub(crate) pending_pool_size_bytes: Gauge,

    /// Number of transactions in the basefee sub-pool
    pub(crate) basefee_pool_transactions: Gauge,
    /// Total amount of memory used by the transactions in the basefee sub-pool in bytes
    pub(crate) basefee_pool_size_bytes: Gauge,

    /// Number of transactions in the queued sub-pool
    pub(crate) queued_pool_transactions: Gauge,
    /// Total amount of memory used by the transactions in the queued sub-pool in bytes
    pub(crate) queued_pool_size_bytes: Gauge,

    /// Number of transactions in the blob sub-pool
    pub(crate) blob_pool_transactions: Gauge,
    /// Total amount of memory used by the transactions in the blob sub-pool in bytes
    pub(crate) blob_pool_size_bytes: Gauge,

    /// Number of all transactions of all sub-pools: pending + basefee + queued + blob
    pub(crate) total_transactions: Gauge,
    /// Number of all legacy transactions in the pool
    pub(crate) total_legacy_transactions: Gauge,
    /// Number of all EIP-2930 transactions in the pool
    pub(crate) total_eip2930_transactions: Gauge,
    /// Number of all EIP-1559 transactions in the pool
    pub(crate) total_eip1559_transactions: Gauge,
    /// Number of all EIP-4844 transactions in the pool
    pub(crate) total_eip4844_transactions: Gauge,
    /// Number of all EIP-7702 transactions in the pool
    pub(crate) total_eip7702_transactions: Gauge,

    /// How often the pool was updated after the canonical state changed
    pub(crate) performed_state_updates: Counter,

    /// Counter for the number of pending transactions evicted
    pub(crate) pending_transactions_evicted: Counter,
    /// Counter for the number of basefee transactions evicted
    pub(crate) basefee_transactions_evicted: Counter,
    /// Counter for the number of blob transactions evicted
    pub(crate) blob_transactions_evicted: Counter,
    /// Counter for the number of queued transactions evicted
    pub(crate) queued_transactions_evicted: Counter,
}

/// Transaction pool blobstore metrics.
///
/// This is a direct copy of [`reth_transaction_pool::metrics::BlobStoreMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "transaction_pool")]
pub struct BlobStoreMetrics {
    /// Number of failed inserts into the blobstore
    pub(crate) blobstore_failed_inserts: Counter,
    /// Number of failed deletes into the blobstore
    pub(crate) blobstore_failed_deletes: Counter,
    /// The number of bytes the blobs in the blobstore take up
    pub(crate) blobstore_byte_size: Gauge,
    /// How many blobs are currently in the blobstore
    pub(crate) blobstore_entries: Gauge,
}

/// All Transactions metrics.
///
/// This is a direct copy of [`reth_transaction_pool::metrics::AllTransactionsMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "transaction_pool")]
pub struct AllTransactionsMetrics {
    /// Number of all transactions by hash in the pool
    pub(crate) all_transactions_by_hash: Gauge,
    /// Number of all transactions by id in the pool
    pub(crate) all_transactions_by_id: Gauge,
    /// Number of all transactions by all senders in the pool
    pub(crate) all_transactions_by_all_senders: Gauge,
    /// Number of blob transactions nonce gaps.
    pub(crate) blob_transactions_nonce_gaps: Counter,
    /// The current blob base fee
    pub(crate) blob_base_fee: Gauge,
    /// The current base fee
    pub(crate) base_fee: Gauge,
}

/// Transaction pool validation metrics.
///
/// This is a direct copy of [`reth_transaction_pool::metrics::TxPoolValidationMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "transaction_pool")]
pub struct TxPoolValidationMetrics {
    /// How long to successfully validate a blob
    #[metrics(buckets = Buckets::LATENCIES)]
    pub(crate) blob_validation_duration: Histogram,
}

/// Transaction pool validator task metrics.
///
/// This is a direct copy of [`reth_transaction_pool::metrics::TxPoolValidatorMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "transaction_pool")]
pub struct TxPoolValidatorMetrics {
    /// Number of in-flight validation job sends waiting for channel capacity
    pub(crate) inflight_validation_jobs: Gauge,
}

#[vise::register]
pub(crate) static TRANSACTION_POOL_METRICS: vise::Global<TxPoolMetrics> = vise::Global::new();
#[vise::register]
pub(crate) static BLOB_STORE_METRICS: vise::Global<BlobStoreMetrics> = vise::Global::new();
#[vise::register]
pub(crate) static ALL_TRANSACTIONS_POOL_METRICS: vise::Global<AllTransactionsMetrics> =
    vise::Global::new();
#[vise::register]
pub(crate) static VALIDATION_POOL_METRICS: vise::Global<TxPoolValidationMetrics> =
    vise::Global::new();
#[vise::register]
pub(crate) static VALIDATOR_POOL_METRICS: vise::Global<TxPoolValidatorMetrics> =
    vise::Global::new();

/// A recorder that wraps `vise` metrics as into `metrics`-compatible structs.
pub(crate) struct ViseRecorder;

impl Recorder for ViseRecorder {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        // Do nothing as descriptions are already provided by vise
    }

    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        // Do nothing as descriptions are already provided by vise
    }

    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        // Do nothing as descriptions are already provided by vise
    }

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> metrics::Counter {
        let counter = match key.name() {
            "transaction_pool.inserted_transactions" => {
                &TRANSACTION_POOL_METRICS.inserted_transactions
            }
            "transaction_pool.invalid_transactions" => {
                &TRANSACTION_POOL_METRICS.invalid_transactions
            }
            "transaction_pool.removed_transactions" => {
                &TRANSACTION_POOL_METRICS.removed_transactions
            }
            "transaction_pool.performed_state_updates" => {
                &TRANSACTION_POOL_METRICS.performed_state_updates
            }
            "transaction_pool.pending_transactions_evicted" => {
                &TRANSACTION_POOL_METRICS.pending_transactions_evicted
            }
            "transaction_pool.basefee_transactions_evicted" => {
                &TRANSACTION_POOL_METRICS.basefee_transactions_evicted
            }
            "transaction_pool.blob_transactions_evicted" => {
                &TRANSACTION_POOL_METRICS.blob_transactions_evicted
            }
            "transaction_pool.queued_transactions_evicted" => {
                &TRANSACTION_POOL_METRICS.queued_transactions_evicted
            }
            // Blob store counters
            "transaction_pool.blobstore_failed_inserts" => {
                &BLOB_STORE_METRICS.blobstore_failed_inserts
            }
            "transaction_pool.blobstore_failed_deletes" => {
                &BLOB_STORE_METRICS.blobstore_failed_deletes
            }
            // All transactions counters
            "transaction_pool.blob_transactions_nonce_gaps" => {
                &ALL_TRANSACTIONS_POOL_METRICS.blob_transactions_nonce_gaps
            }
            _ => {
                tracing::warn!(?key, "unknown counter metric");
                return metrics::Counter::noop();
            }
        };
        metrics::Counter::from_arc(Arc::new(ViseCounter(counter.clone())))
    }

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> metrics::Gauge {
        let gauge = match key.name() {
            "transaction_pool.pending_pool_transactions" => {
                &TRANSACTION_POOL_METRICS.pending_pool_transactions
            }
            "transaction_pool.pending_pool_size_bytes" => {
                &TRANSACTION_POOL_METRICS.pending_pool_size_bytes
            }
            "transaction_pool.basefee_pool_transactions" => {
                &TRANSACTION_POOL_METRICS.basefee_pool_transactions
            }
            "transaction_pool.basefee_pool_size_bytes" => {
                &TRANSACTION_POOL_METRICS.basefee_pool_size_bytes
            }
            "transaction_pool.queued_pool_transactions" => {
                &TRANSACTION_POOL_METRICS.queued_pool_transactions
            }
            "transaction_pool.queued_pool_size_bytes" => {
                &TRANSACTION_POOL_METRICS.queued_pool_size_bytes
            }
            "transaction_pool.blob_pool_transactions" => {
                &TRANSACTION_POOL_METRICS.blob_pool_transactions
            }
            "transaction_pool.blob_pool_size_bytes" => {
                &TRANSACTION_POOL_METRICS.blob_pool_size_bytes
            }
            "transaction_pool.total_transactions" => &TRANSACTION_POOL_METRICS.total_transactions,
            "transaction_pool.total_legacy_transactions" => {
                &TRANSACTION_POOL_METRICS.total_legacy_transactions
            }
            "transaction_pool.total_eip2930_transactions" => {
                &TRANSACTION_POOL_METRICS.total_eip2930_transactions
            }
            "transaction_pool.total_eip1559_transactions" => {
                &TRANSACTION_POOL_METRICS.total_eip1559_transactions
            }
            "transaction_pool.total_eip4844_transactions" => {
                &TRANSACTION_POOL_METRICS.total_eip4844_transactions
            }
            "transaction_pool.total_eip7702_transactions" => {
                &TRANSACTION_POOL_METRICS.total_eip7702_transactions
            }
            // Blob store gauges
            "transaction_pool.blobstore_byte_size" => &BLOB_STORE_METRICS.blobstore_byte_size,
            "transaction_pool.blobstore_entries" => &BLOB_STORE_METRICS.blobstore_entries,
            // All transactions gauges
            "transaction_pool.all_transactions_by_hash" => {
                &ALL_TRANSACTIONS_POOL_METRICS.all_transactions_by_hash
            }
            "transaction_pool.all_transactions_by_id" => {
                &ALL_TRANSACTIONS_POOL_METRICS.all_transactions_by_id
            }
            "transaction_pool.all_transactions_by_all_senders" => {
                &ALL_TRANSACTIONS_POOL_METRICS.all_transactions_by_all_senders
            }
            "transaction_pool.blob_base_fee" => &ALL_TRANSACTIONS_POOL_METRICS.blob_base_fee,
            "transaction_pool.base_fee" => &ALL_TRANSACTIONS_POOL_METRICS.base_fee,
            // Validation gauges
            "transaction_pool.inflight_validation_jobs" => {
                &VALIDATOR_POOL_METRICS.inflight_validation_jobs
            }
            _ => {
                tracing::warn!(?key, "unknown gauge metric");
                return metrics::Gauge::noop();
            }
        };
        metrics::Gauge::from_arc(Arc::new(ViseGauge(gauge.clone())))
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> metrics::Histogram {
        let gauge = match key.name() {
            "transaction_pool.blob_validation_duration" => {
                &VALIDATION_POOL_METRICS.blob_validation_duration
            }
            _ => {
                tracing::warn!(?key, "unknown histogram metric");
                return metrics::Histogram::noop();
            }
        };
        metrics::Histogram::from_arc(Arc::new(ViseHistogram(gauge.clone())))
    }
}

/// A wrapper around `vise::Counter` that implements `metrics::CounterFn`.
struct ViseCounter(Counter);

impl CounterFn for ViseCounter {
    fn increment(&self, value: u64) {
        self.0.inc_by(value);
    }

    fn absolute(&self, _value: u64) {
        tracing::warn!("tried to set metric counter to absolute value; this is not supported");
    }
}

/// A wrapper around `vise::Gauge` that implements `metrics::GaugeFn`.
struct ViseGauge(Gauge);

impl GaugeFn for ViseGauge {
    fn increment(&self, value: f64) {
        self.0.inc_by(value.floor() as i64);
    }

    fn decrement(&self, value: f64) {
        self.0.dec_by(value.floor() as i64);
    }

    fn set(&self, value: f64) {
        self.0.set(value.floor() as i64);
    }
}

/// A wrapper around `vise::Histogram` that implements `metrics::HistogramFn`.
struct ViseHistogram(Histogram);

impl HistogramFn for ViseHistogram {
    fn record(&self, value: f64) {
        self.0.observe(value);
    }
}
