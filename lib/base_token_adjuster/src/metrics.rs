use std::time::Duration;

use vise::{
    Buckets, EncodeLabelSet, EncodeLabelValue, Family, Gauge, Histogram, LabeledFamily, Metrics,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelSet, EncodeLabelValue)]
#[metrics(label = "operation_result", rename_all = "snake_case")]
pub(super) enum OperationResult {
    Success,
    Failure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelSet)]
pub(crate) struct OperationResultLabels {
    pub result: OperationResult,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "base_token_adjuster")]
pub(crate) struct BaseTokenAdjusterMetrics {
    /// Price of the base token in USD.
    #[metrics(labels = ["token_id"])]
    pub token_price: LabeledFamily<String, Gauge<f64>, 1>,
    /// External price API latency.
    #[metrics(buckets = Buckets::LATENCIES)]
    pub external_price_api_latency: Family<OperationResultLabels, Histogram<Duration>>,
    /// Token multiplier setter's balance.
    pub l1_updater_balance: Gauge<f64>,
    /// Used to report token multiplier setter's address. Gauge is always set to one.
    #[metrics(labels = ["address"])]
    pub l1_updater_address: LabeledFamily<String, Gauge, 1>,
    /// Ratio of ETH:base token, that is set on L1.
    pub ratio_l1: Gauge<f64>,
    /// L1 Transaction fee in Ether (i.e. total cost of `setTokenMultiplier`).
    #[metrics(buckets = Buckets::exponential(0.0001..=100.0, 3.0))]
    pub l1_transaction_fee_ether: Histogram<f64>,
    /// Total L1 gas used by L1 transaction (i.e. `setTokenMultiplier`).
    #[metrics(buckets = Buckets::exponential(1.0..=10_000_000.0, 3.0))]
    pub gas_used: Histogram<u64>,
    /// Last nonce used.
    pub l1_updater_nonce: Gauge<u64>,
}

#[vise::register]
pub(crate) static METRICS: vise::Global<BaseTokenAdjusterMetrics> = vise::Global::new();
