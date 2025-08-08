use std::time::Duration;
use vise::EncodeLabelValue;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};

// todo: these metrics are used throughout the batcher subsystem - not only l1 sender
//       we will move them to `batcher_metrics` or `batcher` crate once we have one.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "stage", rename_all = "snake_case")]
pub enum BatchExecutionStage {
    Sealed,
    ProverInputStarted,
    ProverInputGenerated,
    FriProverPicked,
    FriProvedReal,
    FriProvedFake,
    FriProofStored,
    CommitL1TxSent,
    CommitL1TxMined,
    SnarkProverPicked,
    SnarkProvedReal,
    SnarkProvedFake,
    ProveL1TxSent,
    ProveL1TxMined,
    ExecuteL1TxSent,
    ExecuteL1TxMined,
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
}

#[vise::register]
pub(crate) static BATCHER_METRICS: vise::Global<BatcherSubsystemMetrics> = vise::Global::new();
