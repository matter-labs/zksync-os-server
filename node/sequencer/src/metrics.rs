use vise::{Counter, Gauge, LabeledFamily, Metrics};

/// Used for metrics that need to be populated from different components
/// Should be used sparringly
#[derive(Debug, Metrics)]
pub struct GeneralMetrics {
    #[metrics(labels = ["stage"])]
    pub block_number: LabeledFamily<&'static str, Gauge<u64>>,

    #[metrics(labels = ["command"])]
    pub executed_transactions: LabeledFamily<&'static str, Counter>,
}

#[vise::register]
pub(crate) static GENERAL_METRICS: vise::Global<GeneralMetrics> = vise::Global::new();
