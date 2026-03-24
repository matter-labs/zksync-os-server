use crate::execution::execute_block_in_vm::SealReason;
use std::time::Duration;
use vise::Counter;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};

#[derive(Debug, Metrics)]
#[metrics(prefix = "execution")]
pub struct ExecutionMetrics {
    pub block_number: Gauge<u64>,

    #[metrics(unit = Unit::Seconds, buckets = Buckets::exponential(0.1..=600.0, 2.0))]
    pub time_since_last_block: Histogram<Duration>,

    #[metrics(labels = ["seal_reason"])]
    pub seal_reason: LabeledFamily<SealReason, Counter>,

    #[metrics(unit = Unit::Seconds, labels = ["measure"], buckets = Buckets::exponential(0.0000001..=1.0, 2.0))]
    pub tx_execution: LabeledFamily<&'static str, Histogram<Duration>>,

    #[metrics(buckets = Buckets::exponential(1.0..=10_000.0, 2.0))]
    pub transactions_per_block: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=5_000_000.0, 4.0))]
    pub transaction_gas_used: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=50_000_000.0, 4.0))]
    pub transaction_native_used: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=50_000_000.0, 4.0))]
    pub transaction_computation_native_used: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(1.0..=1_000_000.0, 4.0))]
    pub transaction_pubdata_used: Histogram<u64>,

    #[metrics(labels = ["status"])]
    pub transaction_status: LabeledFamily<&'static str, Counter>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=1_000_000_000.0, 4.0))]
    pub computational_native_used_per_block: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=100_000_000.0, 4.0))]
    pub gas_per_block: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(1_000.0..=500_000.0, 4.0))]
    pub pubdata_per_block: Histogram<u64>,

    pub executed_transactions: Counter,

    #[metrics(buckets = Buckets::exponential(1.0..=1_000.0, 1.7))]
    pub storage_writes_per_block: Histogram<u64>,

    pub next_l1_priority_id: Gauge<u64>,

    pub last_execution_version: Gauge<u64>,

    pub pubdata_price: Gauge<u64>,

    pub blob_fill_ratio: Gauge<f64>,

    pub base_fee: Gauge<u64>,

    pub native_price: Gauge<u64>,
}

#[vise::register]
pub(crate) static EXECUTION_METRICS: vise::Global<ExecutionMetrics> = vise::Global::new();
