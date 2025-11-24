use std::time::Duration;
use vise::{Buckets, EncodeLabelValue, Histogram, LabeledFamily, Metrics, Unit};

#[derive(Debug, Metrics)]
#[metrics(prefix = "prover")]
pub struct ProverMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["stage", "type", "id"], buckets = Buckets::LATENCIES)]
    pub prove_time: LabeledFamily<(ProverStage, ProverType, &'static str), Histogram<Duration>, 3>,
    #[metrics(unit = Unit::Seconds, labels = ["stage", "type", "id"], buckets = Buckets::LATENCIES)]
    pub prove_time_per_tx:
        LabeledFamily<(ProverStage, ProverType, &'static str), Histogram<Duration>, 3>,
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

#[vise::register]
pub(crate) static PROVER_METRICS: vise::Global<ProverMetrics> = vise::Global::new();

#[vise::register]
pub(crate) static PROVER_API_METRICS: vise::Global<ProverApiMetrics> = vise::Global::new();
