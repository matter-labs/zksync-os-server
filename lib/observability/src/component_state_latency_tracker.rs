use crate::metrics::GENERAL_METRICS;
use std::fmt::{Debug, Formatter};
use std::hash::Hash;
use std::time::Instant;
use vise::{Counter, EncodeLabelValue, LabeledFamily};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "state", rename_all = "snake_case")]
pub enum GenericComponentState {
    WaitingRecv,
    Processing,
    WaitingSend,
}

/// Helper to track the time spent in each stage of a component.
/// Simple use case: for a Read -> Process -> Sent component:
/// * while reading/waiting for data: GenericComponentState::WaitingRecv
/// * while processing: GenericComponentState::Processing
/// * while sending or backpressure: GenericComponentState::WaitingSend.
///
/// This provides an overview on bottleneck and stuck components.
/// Optionally, component-specific metrics may be provided in `component_specific_metrics`
/// along with component-specific states enum (`S` type param).
/// If it's provided, both `component_specific_metrics`
/// and generic `GENERAL_METRICS.component_time_spent_in_state` will be updated.
pub struct ComponentStateLatencyTracker<S = GenericComponentState>
where
    S: Into<GenericComponentState> + 'static + Debug + Copy + Eq + Hash + Send + Sync,
{
    current_state: S,
    last_stage_started_at: Instant,
    component_name: &'static str,
    component_specific_metrics: Option<&'static LabeledFamily<S, Counter<f64>>>,
}

impl<S> ComponentStateLatencyTracker<S>
where
    S: Into<GenericComponentState> + 'static + Debug + Copy + Eq + Hash + Send + Sync,
{
    pub fn new(
        component_name: &'static str,
        initial_state: S,
        component_specific_metrics: Option<&'static LabeledFamily<S, Counter<f64>>>,
    ) -> Self {
        Self {
            current_state: initial_state,
            last_stage_started_at: Instant::now(),
            component_name,
            component_specific_metrics,
        }
    }

    pub fn enter_state(&mut self, new_state: S) {
        let now = Instant::now();
        let prev_state = self.current_state;
        let duration = now.duration_since(self.last_stage_started_at);

        // Update generic metrics
        GENERAL_METRICS.component_time_spent_in_state[&(self.component_name, prev_state.into())]
            .inc_by(duration.as_secs_f64());

        // Update component-specific metrics if provided
        if let Some(metrics) = self.component_specific_metrics {
            metrics[&prev_state].inc_by(duration.as_secs_f64());
        }

        // Transition
        self.current_state = new_state;
        self.last_stage_started_at = now;
    }
}
impl<S> Debug for ComponentStateLatencyTracker<S>
where
    S: Into<GenericComponentState> + 'static + Debug + Copy + Eq + Hash + Send + Sync,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentStateLatencyTracker")
            .field("component_name", &self.component_name)
            .field("current_state", &self.current_state)
            .field("in_state_for", &self.last_stage_started_at.elapsed())
            .finish()
    }
}
