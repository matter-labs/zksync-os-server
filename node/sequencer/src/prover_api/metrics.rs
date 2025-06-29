use std::time::Duration;
use vise::{Buckets, Histogram, LabeledFamily, Metrics, Unit};

#[derive(Debug, Metrics)]
#[metrics(prefix = "prover")]
pub struct ProverMetrics {
    #[metrics(unit = Unit::Seconds, labels = ["prover"], buckets = Buckets::LATENCIES)]
    pub prove_time: LabeledFamily<&'static str, Histogram<Duration>>,
}
#[vise::register]
pub(crate) static PROVER_METRICS: vise::Global<ProverMetrics> = vise::Global::new();
