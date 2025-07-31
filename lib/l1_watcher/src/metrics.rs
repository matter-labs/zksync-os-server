use alloy::primitives::BlockNumber;
use vise::{Counter, Gauge, Metrics};

#[derive(Debug, Metrics)]
pub struct L1Metrics {
    pub next_l1_priority_id: Gauge<u64>,
    pub most_recently_scanned_l1_block: Gauge<BlockNumber>,
    pub l1_transactions_loaded: Counter,
}

#[vise::register]
pub static L1_METRICS: vise::Global<L1Metrics> = vise::Global::new();
