use crate::config::ComponentId;
use vise::{Counter, EncodeLabelSet, Family, Gauge, Metrics};

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
pub struct ComponentLabel {
    pub component: &'static str,
}

impl From<ComponentId> for ComponentLabel {
    fn from(id: ComponentId) -> Self {
        Self {
            component: id.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
pub struct DirectionLabel {
    /// "open" when backpressure starts, "cleared" when it ends.
    pub direction: &'static str,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "pipeline")]
pub struct MonitorMetrics {
    /// 1 if this component is currently an active backpressure cause, else 0.
    pub backpressure_active: Family<ComponentLabel, Gauge<u64>>,
    /// Blocks behind pipeline head.
    pub component_block_lag: Family<ComponentLabel, Gauge<u64>>,
    /// Block-timestamp lag in seconds (0 if timestamp unavailable for head or component).
    pub component_time_lag_seconds: Family<ComponentLabel, Gauge<f64>>,
    /// Last block number successfully processed by this component.
    pub component_last_processed_block: Family<ComponentLabel, Gauge<u64>>,
    /// Number of items queued in this component's output channel.
    pub channel_queue_depth: Family<ComponentLabel, Gauge<u64>>,
    /// Counts transitions into "not accepting" (direction="open") and back to accepting (direction="cleared").
    /// vise appends _total automatically for Counter types.
    pub acceptance_state_changes: Family<DirectionLabel, Counter<u64>>,
}

#[vise::register]
pub static MONITOR_METRICS: vise::Global<MonitorMetrics> = vise::Global::new();
