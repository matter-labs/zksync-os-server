use std::time::Duration;
use vise::{Buckets, Histogram, Metrics, Unit};

#[derive(Debug, Metrics)]
#[metrics(prefix = "prover")]
pub struct ProverMetrics {
    #[metrics(unit = Unit::Seconds, buckets = Buckets::LATENCIES)]
    pub prove_time: Histogram<Duration>,
}
#[vise::register]
pub(crate) static PROVER_METRICS: vise::Global<ProverMetrics> = vise::Global::new();
