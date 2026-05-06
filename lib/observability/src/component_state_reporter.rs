use crate::generic_component_state::GenericComponentState;
use crate::metrics::GENERAL_METRICS;
use crate::state_label::StateLabel;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// Coordinates for a pipeline item
#[derive(Clone, Debug)]
pub struct TrackingCoordinates {
    pub block_number: u64,
    pub timestamp: Option<u64>,
    pub batch_number: Option<u64>,
}

/// State snapshot reported by a pipeline component on every state transition.
#[derive(Clone, Debug)]
pub struct ComponentState {
    /// Component state - Idle or Active.
    pub state: GenericComponentState,

    /// Fine-grained state label.
    pub specific_state: &'static str,

    /// When the current state was entered.
    pub state_entered_at: Instant,

    /// Last item picked from the input channel.
    pub picked: Option<TrackingCoordinates>,

    /// Last item fully handled/forwarded downstream.
    pub processed: Option<TrackingCoordinates>,
}

#[derive(Debug, Clone)]
pub struct ComponentStateReporter {
    component: &'static str,
    sender: watch::Sender<ComponentState>,
    state_tx: mpsc::Sender<(GenericComponentState, &'static str)>,
}

impl ComponentStateReporter {
    /// Returns the reporter (owned by the component) and the receiver (handed to the monitor).
    pub fn new(component: &'static str) -> (Self, watch::Receiver<ComponentState>) {
        let initial = ComponentState {
            state: GenericComponentState::Idle,
            specific_state: "idle",
            state_entered_at: Instant::now(),
            picked: None,
            processed: None,
        };
        let (sender, receiver) = watch::channel(initial);
        let (state_tx, state_rx) = mpsc::channel(512);
        tokio::spawn(flush_state_time(
            component,
            state_rx,
            GenericComponentState::Idle,
            "idle",
        ));
        (
            Self {
                component,
                sender,
                state_tx,
            },
            receiver,
        )
    }

    /// Transition to a new state.
    pub fn enter_state(&self, new_state: impl StateLabel) {
        let now = Instant::now();
        let new_generic = new_state.generic();
        let new_specific = new_state.specific();
        let mut transitioned = false;
        self.sender.send_modify(|state| {
            if state.specific_state == new_specific {
                return;
            }
            transitioned = true;
            state.state = new_generic;
            state.specific_state = new_specific;
            state.state_entered_at = now;
        });
        if transitioned {
            let _ = self.state_tx.try_send((new_generic, new_specific));
        }
    }

    /// Record when an item was dequeued from the input channel (before any processing).
    pub fn record_picked(
        &self,
        block_number: u64,
        timestamp: Option<u64>,
        batch_number: Option<u64>,
    ) {
        let mut highest_seen: Option<u64> = None;
        self.sender.send_if_modified(|state| {
            let stale = state
                .picked
                .as_ref()
                .is_some_and(|c| block_number < c.block_number);
            if stale {
                highest_seen = state.picked.as_ref().map(|c| c.block_number);
                return false;
            }
            // On the first pick, seed processed one behind so the monitor sees a non-zero
            // diff immediately - without this, processed stays None and the fallback
            // `processed.or(picked)` yields diff=0 for components that never call
            // record_processed (e.g. provers when disabled).
            if state.processed.is_none() {
                state.processed = Some(TrackingCoordinates {
                    block_number: block_number.saturating_sub(1),
                    timestamp,
                    batch_number: batch_number.map(|n| n.saturating_sub(1)),
                });
            }
            state.picked = Some(TrackingCoordinates {
                block_number,
                timestamp,
                batch_number,
            });
            true
        });
        let component = self.component;
        if let Some(highest_seen) = highest_seen {
            if let Some(batch) = batch_number {
                tracing::info!(
                    component,
                    "picked batch={batch} last_block={block_number} (out of order, highest_seen={highest_seen})"
                );
            } else {
                tracing::info!(
                    component,
                    "picked block={block_number} (out of order, highest_seen={highest_seen})"
                );
            }
        } else if let Some(batch) = batch_number {
            tracing::info!(component, "picked batch={batch} last_block={block_number}");
        } else {
            tracing::info!(component, "picked block={block_number}");
        }
    }

    /// Record when an item was fully processed.
    pub fn record_processed(
        &self,
        block_number: u64,
        timestamp: Option<u64>,
        batch_number: Option<u64>,
    ) {
        let mut highest_seen: Option<u64> = None;
        self.sender.send_if_modified(|state| {
            let stale = state
                .processed
                .as_ref()
                .is_some_and(|c| block_number < c.block_number);
            if stale {
                highest_seen = state.processed.as_ref().map(|c| c.block_number);
                return false;
            }
            state.processed = Some(TrackingCoordinates {
                block_number,
                timestamp,
                batch_number,
            });
            true
        });
        let component = self.component;
        if let Some(highest_seen) = highest_seen {
            if let Some(batch) = batch_number {
                tracing::info!(
                    component,
                    "processed batch={batch} last_block={block_number} (out of order, highest_seen={highest_seen})"
                );
            } else {
                tracing::info!(
                    component,
                    "processed block={block_number} (out of order, highest_seen={highest_seen})"
                );
            }
        } else if let Some(batch) = batch_number {
            tracing::info!(
                component,
                "processed batch={batch} last_block={block_number}"
            );
        } else {
            tracing::info!(component, "processed block={block_number}");
        }
    }
}

/// Runs as a per-component background task. Continuously increments
/// `component_time_spent_in_state` on every 2-second tick and on every state.
async fn flush_state_time(
    component: &'static str,
    mut rx: mpsc::Receiver<(GenericComponentState, &'static str)>,
    initial_state: GenericComponentState,
    initial_specific: &'static str,
) {
    const TICK: Duration = Duration::from_secs(2);
    let mut tracked_state = initial_state;
    let mut tracked_specific = initial_specific;
    let mut last_flush = Instant::now();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = Instant::now();
                let elapsed = now.duration_since(last_flush).as_secs_f64();
                if elapsed > 0.0 {
                    GENERAL_METRICS.component_time_spent_in_state
                        [&(component, tracked_state, tracked_specific)]
                        .inc_by(elapsed);
                }
                last_flush = now;
            }
            msg = rx.recv() => {
                let Some((new_state, new_specific)) = msg else { return };
                let now = Instant::now();
                let elapsed = now.duration_since(last_flush).as_secs_f64();
                if elapsed > 0.0 {
                    GENERAL_METRICS.component_time_spent_in_state
                        [&(component, tracked_state, tracked_specific)]
                        .inc_by(elapsed);
                }
                tracked_state = new_state;
                tracked_specific = new_specific;
                last_flush = now;
            }
        }
    }
}
