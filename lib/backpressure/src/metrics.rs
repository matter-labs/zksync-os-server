use crate::config::ComponentId;
use vise::{Counter, Family, Gauge, Metrics};

#[derive(Debug, Metrics)]
#[metrics(prefix = "pipeline")]
pub struct MonitorMetrics {
    /// 1 if this component is currently an active backpressure cause, else 0.
    pub backpressure_active: Family<ComponentId, Gauge<u64>>,
    /// Blocks behind pipeline head.
    pub component_block_diff_to_head: Family<ComponentId, Gauge<u64>>,
    /// Block-timestamp lag in seconds vs pipeline head (0 if timestamp unavailable for head or component).
    pub component_time_diff_to_head_seconds: Family<ComponentId, Gauge<f64>>,
    /// Last block number dequeued from the input channel by this component.
    pub component_last_picked_block: Family<ComponentId, Gauge<u64>>,
    /// Last block number successfully processed by this component.
    pub component_last_processed_block: Family<ComponentId, Gauge<u64>>,
    /// Processing lag in blocks between this component and its upstream neighbour.
    /// Computed as upstream.block_processed.block_number − this.block_processed.block_number.
    pub component_block_diff_to_upstream: Family<ComponentId, Gauge<u64>>,
    /// Processing lag in seconds between this component and its upstream neighbour.
    /// Computed as upstream.block_processed.timestamp − this.block_processed.timestamp.
    /// 0 if either timestamp is unavailable.
    pub component_time_diff_to_upstream_seconds: Family<ComponentId, Gauge<f64>>,
    /// Processing lag in batches between this component and its upstream neighbour.
    /// Computed as upstream.batch_processed − this.batch_processed.
    /// Only set for batch-pipeline components with batch tracking.
    pub component_batch_diff_to_upstream: Family<ComponentId, Gauge<u64>>,
    /// Last batch number fully processed by this component.
    /// Only set for batch-pipeline components that call `record_processed` with a batch arg.
    pub component_last_processed_batch: Family<ComponentId, Gauge<u64>>,
    /// Last batch number dequeued from the input channel by this component.
    /// Only set for batch-pipeline components that call `record_picked` with a batch arg.
    pub component_last_picked_batch: Family<ComponentId, Gauge<u64>>,
    /// Counts transitions from Accepting to NotAccepting (transaction acceptance suspended).
    pub acceptance_state_changes: Counter<u64>,
    /// Counts transitions from NotAccepting to Accepting (backpressure cleared).
    /// Paired with acceptance_state_changes so operators can track both sides.
    pub acceptance_state_clears: Counter<u64>,
    /// 1 if the monitor is currently accepting transactions, 0 if backpressure is active.
    pub accepting: Gauge<u64>,
    /// Configured `max_block_diff_to_upstream` for this component. Emitted once at startup and
    /// only for components with a Some threshold — absence of the series means "not configured".
    pub backpressure_threshold_block_diff_to_upstream: Family<ComponentId, Gauge<u64>>,
    /// Configured `max_time_diff_to_upstream` for this component in seconds. Same emit policy.
    pub backpressure_threshold_time_diff_to_upstream_seconds: Family<ComponentId, Gauge<f64>>,
    /// Configured `max_batch_diff_to_upstream` for this component. Same emit policy.
    pub backpressure_threshold_batch_diff_to_upstream: Family<ComponentId, Gauge<u64>>,
    /// Registration-order index of this component (0 = pipeline head). Emitted once at
    /// monitor startup. Lets Grafana sort tables by a numeric rank instead of hard-coding
    /// a per-panel enum list that drifts whenever a component is added.
    pub component_order: Family<ComponentId, Gauge<u64>>,
}

#[vise::register]
pub static MONITOR_METRICS: vise::Global<MonitorMetrics> = vise::Global::new();
