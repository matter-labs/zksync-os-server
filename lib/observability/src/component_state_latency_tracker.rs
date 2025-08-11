use crate::metrics::{GENERAL_METRICS, GeneralMetrics};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vise::{Counter, EncodeLabelValue, LabeledFamily};

const FLUSHING_INTERVAL: Duration = Duration::from_secs(5);

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
#[derive(Clone)]
pub struct ComponentStateLatencyTracker<S = GenericComponentState>
where
    S: Into<GenericComponentState> + 'static + Debug + Copy + Eq + Hash + Send + Sync,
{
    inner: Arc<Mutex<ComponentStateLatencyTrackerInner<S>>>,
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
            inner: Arc::new(Mutex::new(ComponentStateLatencyTrackerInner::new(
                component_name,
                initial_state,
                component_specific_metrics,
            ))),
        }
    }

    pub fn set_state(&self, state: S) {
        self.inner.lock().unwrap().set_state(state);
    }

    pub fn spawn_flusher(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(FLUSHING_INTERVAL);
            loop {
                ticker.tick().await;
                inner.lock().unwrap().flush();
            }
        });
    }
}
impl<S> Debug for ComponentStateLatencyTracker<S>
where
    S: Into<GenericComponentState> + 'static + Debug + Copy + Eq + Hash + Send + Sync,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.inner.try_lock() {
            Ok(inner) => f
                .debug_struct("ComponentStateLatencyTracker")
                .field("component_name", &inner.component_name)
                .field("current_state", &inner.current_state)
                .field("in_state_for", &inner.last_stage_started_at.elapsed())
                .field("pending_len", &inner.pending_latencies.len())
                .finish(),
            Err(_) => f.write_str("ComponentStateLatencyTracker { <locked> }"),
        }
    }
}

pub struct ComponentStateLatencyTrackerInner<S = GenericComponentState>
where
    S: Into<GenericComponentState> + 'static + Debug + Copy + Eq + Hash + Send + Sync,
{
    current_state: S,
    last_stage_started_at: Instant,
    component_name: &'static str,
    pending_latencies: HashMap<S, Duration>,
    component_specific_metrics: Option<&'static LabeledFamily<S, Counter<f64>>>,
}

impl<S: Into<GenericComponentState> + 'static + Debug + Copy + Eq + Hash + Send + Sync>
    ComponentStateLatencyTrackerInner<S>
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
            pending_latencies: HashMap::new(),
        }
    }
    pub fn set_state(&mut self, state: S) {
        self.flush_current_state_time();
        self.current_state = state;
    }
    pub fn flush(&mut self) {
        self.flush_current_state_time();

        for (state, dur) in self.pending_latencies.drain() {
            GENERAL_METRICS.component_time_spent_in_state[&(self.component_name, state.into())]
                .inc_by(dur.as_secs_f64());
            if let Some(metrics) = self.component_specific_metrics {
                metrics[&state].inc_by(dur.as_secs_f64());
            }
        }
    }
    fn flush_current_state_time(&mut self) {
        use std::ops::AddAssign;

        let duration = self.last_stage_started_at.elapsed();
        self.pending_latencies
            .entry(self.current_state)
            .and_modify(|d| d.add_assign(duration))
            .or_insert(duration);

        self.last_stage_started_at = Instant::now();
    }
}
