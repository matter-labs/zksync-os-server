use std::time::Duration;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use vise::{Counter, EncodeLabelValue};

// todo: these metrics are used throughout the batcher subsystem - not only l1 sender
//       we will move them to `batcher_metrics` or `batcher` crate once we have one.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "stage", rename_all = "snake_case")]
pub enum BatchExecutionStage {
    BatchSealed,
    SigningStarted,
    BatchSigned,
    ProverInputStarted,
    FriProverPicked,
    FriProvedReal,
    FriProvedFake,
    FriProofStored,
    CommitL1TxSent,
    CommitL1TxMined,
    CommitL1Passthrough,
    SnarkProverPicked,
    SnarkProvedReal,
    SnarkProvedFake,
    ProveL1TxSent,
    ProveL1TxMined,
    ProveL1Passthrough,
    ExecuteL1TxSent,
    ExecuteL1TxMined,
    ExecuteL1Passthrough,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "batcher")]
pub struct BatcherSubsystemMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = Buckets::LATENCIES)]
    pub execution_stages: LabeledFamily<BatchExecutionStage, Histogram<Duration>>,

    #[metrics(labels = ["stage"])]
    pub batch_number: LabeledFamily<BatchExecutionStage, Gauge<u64>>,

    #[metrics(labels = ["stage"])]
    pub block_number: LabeledFamily<BatchExecutionStage, Gauge<u64>>,

    #[metrics(labels = ["seal_reason"])]
    pub seal_reason: LabeledFamily<&'static str, Counter>,

    #[metrics(buckets = Buckets::exponential(1.0..=100_000.0, 3.0))]
    pub transactions_per_batch: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(1.0..=1_000.0, 2.0))]
    pub blocks_per_batch: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=10_000_000_000.0, 5.0))]
    pub computational_native_used_per_batch: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(1_000.0..=1_000_000.0, 4.0))]
    pub pubdata_per_batch: Histogram<u64>,
}
#[vise::register]
pub static BATCHER_METRICS: vise::Global<BatcherSubsystemMetrics> = vise::Global::new();
