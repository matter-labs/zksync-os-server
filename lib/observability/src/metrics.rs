use std::time::{SystemTime, UNIX_EPOCH};

use crate::GenericComponentState;
use vise::{Counter, Gauge, LabeledFamily, Metrics};

#[derive(Debug, Metrics)]
pub struct GeneralMetrics {
    /// Counts the number of seconds spent in each state.
    /// `specific_state` tracks component-specific state -
    /// the set of values may be different for different components
    #[metrics(labels = ["component", "generic_state", "specific_state"])]
    pub component_time_spent_in_state:
        LabeledFamily<(&'static str, GenericComponentState, &'static str), Counter<f64>, 3>,

    /// Unix timestamp for when the process was started.
    /// Additionally, labels are used to track the version and role (main node / external node)
    #[metrics(labels = ["version", "role"])]
    pub process_started_at: LabeledFamily<(&'static str, &'static str), Gauge<i64>, 2>,

    /// Time spent on various startup routines.
    #[metrics(labels = ["stage"])]
    pub startup_time: LabeledFamily<&'static str, Gauge<f64>>,

    #[metrics(labels = ["fee_collector_address"])]
    pub fee_collector_address: LabeledFamily<&'static str, Gauge, 1>,

    pub chain_id: Gauge<u64>,

    /// Number of blacklisted addresses in the internal config on server startup
    pub blacklisted_addresses_count: Gauge<usize>,
}

#[vise::register]
pub static GENERAL_METRICS: vise::Global<GeneralMetrics> = vise::Global::new();

#[derive(Debug, Metrics)]
pub struct PushMetrics {
    /// Unix timestamp of the latest unexpected event by event type.
    #[metrics(labels = ["event"])]
    pub unexpected_events_push: LabeledFamily<&'static str, Gauge<i64>, 1>,
}

#[vise::register]
pub static PUSH_METRICS: vise::Global<PushMetrics> = vise::Global::new();

pub fn record_unexpected_event(event: &'static str) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs() as i64;
    PUSH_METRICS.unexpected_events_push[&event].set(timestamp);
}
