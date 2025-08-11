use crate::component_state_latency_tracker::GenericComponentState;
use vise::{Counter, Histogram, LabeledFamily, Metrics};

#[derive(Debug, Metrics)]
pub struct GeneralMetrics {
    /// Counts the number of seconds spent in each state.
    #[metrics(labels = ["component", "state"])]
    pub component_time_spent_in_state:
        LabeledFamily<(&'static str, GenericComponentState), Counter<f64>, 2>,
}

#[vise::register]
pub static GENERAL_METRICS: vise::Global<GeneralMetrics> = vise::Global::new();
