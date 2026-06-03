use alloy::primitives::BlockNumber;
use vise::{Counter, EncodeLabelSet, Family, Gauge, LabeledFamily, Metrics, Unit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelSet)]
pub(crate) struct LogsCacheLabels {
    pub chain_id: u64,
}

#[derive(Debug, Metrics)]
pub struct L1Metrics {
    #[metrics(labels = ["event"])]
    pub most_recently_scanned_l1_block: LabeledFamily<&'static str, Gauge<BlockNumber>>,
    #[metrics(labels = ["event"])]
    pub events_loaded: LabeledFamily<&'static str, Counter>,
    pub logs_cache_hits: Family<LogsCacheLabels, Counter>,
    pub logs_cache_fallbacks: Family<LogsCacheLabels, Counter>,
    pub logs_cache_blocks_loaded: Family<LogsCacheLabels, Counter>,
    #[metrics(unit = Unit::Bytes)]
    pub logs_cache_approx_memory: Family<LogsCacheLabels, Gauge<usize>>,
}

#[vise::register]
pub static METRICS: vise::Global<L1Metrics> = vise::Global::new();
