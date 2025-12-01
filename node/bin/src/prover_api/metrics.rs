use std::time::Duration;
use vise::{Buckets, EncodeLabelValue, Gauge, Histogram, LabeledFamily, Metrics, Unit};

#[derive(Debug, Metrics)]
#[metrics(prefix = "prover")]
pub struct ProverMetrics {
    /// Minimum and maximum batch numbers in the job map (picked or unpicked)
    /// (there may be gaps - so the diff doesn't always equal to .batch_count())
    #[metrics(labels = ["stage"])]
    pub prover_job_map_min_batch_number: LabeledFamily<ProverStage, Gauge>,
    #[metrics(labels = ["stage"])]
    pub prover_job_map_max_batch_number: LabeledFamily<ProverStage, Gauge>,
    #[metrics(labels = ["stage"])]
    /// Total number of batches in ProverMap.
    /// There may be gaps - so prover_job_map_max_batch_number - prover_job_map_min_batch_number
    /// doesn't always equal to .batch_count()
    pub batch_count: LabeledFamily<ProverStage, Gauge>,
    /// The time passed between when a job was picked and reported back
    #[metrics(unit = Unit::Seconds, labels = ["stage", "type", "id"], buckets = Buckets::LATENCIES)]
    pub prove_time: LabeledFamily<(ProverStage, ProverType, String), Histogram<Duration>, 3>,
    /// The time passed between when a job was picked and reported back
    /// divided by the number of transactions in job.
    /// That is, for SNARKs it's divided by the total number of txs in batch range.
    #[metrics(unit = Unit::Seconds, labels = ["stage", "type", "id"], buckets = Buckets::LATENCIES)]
    pub prove_time_per_tx: LabeledFamily<(ProverStage, ProverType, String), Histogram<Duration>, 3>,
    #[metrics(labels = ["stage", "type"], buckets = Buckets::values(&[1.0, 2.0, 3.0, 4.0, 5.0, 10.0, 20.0, 50.0]))]
    pub proved_after_attempts: LabeledFamily<(ProverStage, ProverType), Histogram, 2>,
    /// Time spent waiting to acquire the lock in ProverJobMap
    #[metrics(unit = Unit::Seconds, labels = ["stage", "method"], buckets = Buckets::LATENCIES)]
    pub job_map_lock_acquire_time:
        LabeledFamily<(ProverStage, JobMapMethod), Histogram<Duration>, 2>,
    /// Time spent holding the lock in ProverJobMap
    #[metrics(unit = Unit::Seconds, labels = ["stage", "method"], buckets = Buckets::LATENCIES)]
    pub job_map_lock_hold_time: LabeledFamily<(ProverStage, JobMapMethod), Histogram<Duration>, 2>,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "prover_api")]
pub struct ProverApiMetrics {
    /// Latency for job pick requests
    #[metrics(unit = Unit::Seconds, labels = ["stage", "job_result"], buckets = Buckets::LATENCIES)]
    pub pick_job_latency: LabeledFamily<(ProverStage, PickJobResult), Histogram<Duration>, 2>,
    /// Latency for proof submission requests
    #[metrics(unit = Unit::Seconds, labels = ["stage"], buckets = Buckets::LATENCIES)]
    pub submit_proof_latency: LabeledFamily<ProverStage, Histogram<Duration>>,
    /// Counter for timed-out jobs that were reassigned to another prover
    #[metrics(labels = ["stage"])]
    pub timed_out_jobs_reassigned: LabeledFamily<ProverStage, vise::Counter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "stage", rename_all = "snake_case")]
pub enum ProverStage {
    Fri,
    Snark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "type", rename_all = "snake_case")]
pub enum ProverType {
    Real,
    Fake,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "job_result", rename_all = "snake_case")]
pub enum PickJobResult {
    /// Job was returned (new or timed out)
    NewJob,
    /// No job available
    NoJob,
    /// Request failed with error
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "method", rename_all = "snake_case")]
pub enum JobMapMethod {
    AddJob,
    PickJobsWhile,
    CompleteManyJobs,
    GetJobBatchMetadata,
    GetProverInput,
    Status,
}

#[vise::register]
pub(crate) static PROVER_METRICS: vise::Global<ProverMetrics> = vise::Global::new();

#[vise::register]
pub(crate) static PROVER_API_METRICS: vise::Global<ProverApiMetrics> = vise::Global::new();
