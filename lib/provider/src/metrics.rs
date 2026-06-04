use vise::{Gauge, LabeledFamily, Metrics};

#[derive(Debug, Metrics)]
#[metrics(prefix = "l1_state")]
pub struct L1StateMetrics {
    /// Used to report L1 contract addresses to Prometheus.
    /// Gauge is always set to one.
    #[metrics(labels = ["bridgehub", "diamond_proxy", "validator_timelock"])]
    pub l1_contract_addresses: LabeledFamily<(&'static str, &'static str, &'static str), Gauge, 3>,
    /// Used to report the DA mode (rollup/validium).
    /// Gauge is always set to one.
    #[metrics(labels = ["da_input_mode"])]
    pub da_input_mode: LabeledFamily<&'static str, Gauge, 1>,
}

#[vise::register]
pub static L1_STATE_METRICS: vise::Global<L1StateMetrics> = vise::Global::new();
