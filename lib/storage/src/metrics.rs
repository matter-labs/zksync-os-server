use alloy::primitives::BlockNumber;
use std::time::Duration;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
const LATENCIES: Buckets = Buckets::exponential(0.00001..=5.0, 2.0);
const BLOCK_DATA_SIZES: Buckets = Buckets::exponential(10.0..=10000000.0, 2.0);

#[derive(Debug, Metrics)]
#[metrics(prefix = "repositories")]
pub struct RepositoriesMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub insert_block: LabeledFamily<&'static str, Histogram<Duration>>,
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub insert_block_per_tx: Histogram<Duration>,
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES)]
    pub persist_block: Histogram<Duration>,
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES)]
    pub persist_block_per_tx: Histogram<Duration>,
    pub persistence_lag: Gauge<usize>,
    #[metrics(unit = Unit::Bytes, buckets = BLOCK_DATA_SIZES)]
    pub block_data_size: Histogram<usize>,
    #[metrics(unit = Unit::Bytes, buckets = BLOCK_DATA_SIZES)]
    pub block_data_size_per_tx: Histogram<usize>,
    pub in_memory_txs_count: Gauge<usize>,
    pub persist_block_number: Gauge<BlockNumber>,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "batch_storage")]
pub struct BatchStorageMetrics {
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES)]
    pub persist_latency: Histogram<Duration>,
    #[metrics(unit = Unit::Bytes, buckets = BLOCK_DATA_SIZES)]
    pub data_size: Histogram<usize>,
    pub persist_batch_number: Gauge<u64>,
}

#[vise::register]
pub(crate) static REPOSITORIES_METRICS: vise::Global<RepositoriesMetrics> = vise::Global::new();

#[vise::register]
pub(crate) static BATCH_STORAGE_METRICS: vise::Global<BatchStorageMetrics> = vise::Global::new();
