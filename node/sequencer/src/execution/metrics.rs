use std::time::Duration;
use vise::{Buckets, Counter, Gauge, Histogram, LabeledFamily, Metrics, Unit};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
const STORAGE_WRITES: Buckets = Buckets::exponential(1.0..=1000.0, 1.7);
//todo: refactor execution_metrics

#[derive(Debug, Metrics)]
pub struct ExecutionMetrics {
    // todo: maybe split off performance metrics into a separate struct?
    #[metrics(labels = ["command"])]
    pub executed_transactions: LabeledFamily<&'static str, Counter>,

    #[metrics(labels = ["stage"])]
    pub sealed_block: LabeledFamily<&'static str, Gauge<u64>>,

    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = LATENCIES_FAST)]
    pub block_execution_stages: LabeledFamily<&'static str, Histogram<Duration>>,

    #[metrics(buckets = STORAGE_WRITES)]
    pub storage_writes_per_block: Histogram<u64>,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "block_replay_storage")]
pub struct BlockReplayRocksDBMetrics {
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub get_latency: Histogram<Duration>,

    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub set_latency: Histogram<Duration>,
}

#[vise::register]
pub(crate) static EXECUTION_METRICS: vise::Global<ExecutionMetrics> = vise::Global::new();

#[vise::register]
pub(crate) static BLOCK_REPLAY_ROCKS_DB_METRICS: vise::Global<BlockReplayRocksDBMetrics> =
    vise::Global::new();
