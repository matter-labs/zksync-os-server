use alloy::primitives::BlockNumber;
use vise::{Counter, Gauge, LabeledFamily, Metrics};

#[derive(Debug, Metrics)]
pub struct L1Metrics {
    #[metrics(labels = ["event"])]
    pub most_recently_scanned_l1_block: LabeledFamily<&'static str, Gauge<BlockNumber>>,
    #[metrics(labels = ["event"])]
    pub events_loaded: LabeledFamily<&'static str, Counter>,
}

#[vise::register]
pub static METRICS: vise::Global<L1Metrics> = vise::Global::new();
