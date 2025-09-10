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
}

#[vise::register]
pub static GENERAL_METRICS: vise::Global<GeneralMetrics> = vise::Global::new();
