use crate::GenericComponentState;
use vise::{Counter, LabeledFamily, Metrics};

#[derive(Debug, Metrics)]
pub struct GeneralMetrics {
    /// Counts the number of seconds spent in each state.
    /// `specific_state` tracks component-specific state -
    /// the set of values may be different for different components
    #[metrics(labels = ["component", "generic_state", "specific_state"])]
    pub component_time_spent_in_state:
        LabeledFamily<(&'static str, GenericComponentState, &'static str), Counter<f64>, 3>,
}

#[vise::register]
pub static GENERAL_METRICS: vise::Global<GeneralMetrics> = vise::Global::new();
