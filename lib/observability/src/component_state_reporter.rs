//!
//! Component-state reporter: centralized accounting of time spent by components in states.
//! - Components create a lightweight handle via `ComponentStateReporter::global().handle_for(...)`.
//! - The handle sends EnterState events over an async mpsc channel.
//! - A background task periodically (every TICK_SECS) increments a single metric family
//!   `component_time_spent_in_state[component, GenericComponentState, specific_state]` with
//!   time spent in the current state. Transitions are also finalized immediately on EnterState.
//!
//!
use crate::generic_component_state::GenericComponentState;
use crate::metrics::GENERAL_METRICS;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;
use tokio::sync::mpsc::{self, Receiver, Sender};

/// How often to report time spent in the current state for each component.
/// Must be lower than reporting window in Prometheus (usually 15 or 30 seconds)
const TICK_SECS: u64 = 2;

/// Individual component state.
/// Usually an `enum` - each state is reported as:
/// * `GenericComponentState`
/// * `specific` string
pub trait StateLabel: Send + Sync + 'static {
    fn generic(&self) -> GenericComponentState;
    fn specific(&self) -> &'static str;
}

/// Individual messages that are sent to `ComponentStateHandle`
struct ReporterMsg {
    component: &'static str,
    new_label: Box<dyn StateLabel>,
}

/// `ComponentStateHandle` stores this per component
/// to track the current state and time spent in it.
struct RegistryEntry {
    component: &'static str,
    current_state: Box<dyn StateLabel>,
    last_report_at: Instant,
}

impl RegistryEntry {
    fn flush(&mut self) {
        let secs = self.last_report_at.elapsed().as_secs_f64();
        GENERAL_METRICS.component_time_spent_in_state[&(
            self.component,
            self.current_state.generic(),
            self.current_state.specific(),
        )]
            .inc_by(secs);
        self.last_report_at = Instant::now();
    }
}

#[derive(Clone)]
pub struct ComponentStateReporter {
    tx: Sender<ReporterMsg>,
}

impl ComponentStateReporter {
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<ComponentStateReporter> = OnceLock::new();
        INSTANCE.get_or_init(Self::new)
    }

    fn new() -> Self {
        let (tx, rx) = mpsc::channel(512);
        // Spawn background task
        tokio::spawn(run_reporter(rx));
        Self { tx }
    }

    pub fn handle_for<S>(
        &self,
        component: &'static str,
        initial_state: S,
    ) -> ComponentStateHandle<S>
    where
        S: StateLabel,
    {
        let _ = self.tx.try_send(ReporterMsg {
            component,
            new_label: Box::new(initial_state),
        });

        ComponentStateHandle {
            component,
            tx: self.tx.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ComponentStateHandle<S> {
    component: &'static str,
    tx: Sender<ReporterMsg>,
    _marker: std::marker::PhantomData<S>,
}

impl<S: StateLabel> ComponentStateHandle<S> {
    pub fn enter_state(&self, new_state: S) {
        let _ = self.tx.try_send(ReporterMsg {
            component: self.component,
            new_label: Box::new(new_state),
        });
    }
}

async fn run_reporter(mut rx: Receiver<ReporterMsg>) {
    let mut registry: HashMap<&'static str, RegistryEntry> = HashMap::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(TICK_SECS));

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                for (_, entry) in registry.iter_mut() {
                    entry.flush();
                }
            },
            Some(ReporterMsg { component, new_label }) = rx.recv() => {
                if let Some(entry) = registry.get_mut(&component) {
                    // finalize the previous period up until now
                    entry.flush();
                    entry.current_state = new_label;
                } else {
                    registry.insert(component, RegistryEntry {
                        component,
                        current_state: new_label,
                        last_report_at: Instant::now(),
                    });
                }
            }
        }
    }
}
