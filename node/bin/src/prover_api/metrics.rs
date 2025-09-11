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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "stage", rename_all = "snake_case")]
#[allow(dead_code)] // SNARK cannot be tracked yet
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

#[vise::register]
pub(crate) static PROVER_METRICS: vise::Global<ProverMetrics> = vise::Global::new();
