use crate::config::ComponentId;
use vise::{Counter, Family, Gauge, Metrics};

#[derive(Debug, Metrics)]
#[metrics(prefix = "pipeline")]
pub struct MonitorMetrics {
    /// 1 if this component is currently an active backpressure cause, else 0.
    pub backpressure_active: Family<ComponentId, Gauge<u64>>,
    /// Blocks behind pipeline head.
    pub component_block_lag: Family<ComponentId, Gauge<u64>>,
    /// Block-timestamp lag in seconds (0 if timestamp unavailable for head or component).
    pub component_time_lag_seconds: Family<ComponentId, Gauge<f64>>,
    /// Last block number successfully processed by this component.
    pub component_last_processed_block: Family<ComponentId, Gauge<u64>>,
    /// Blocks queued between this component and its upstream neighbour.
    /// Computed as upstream.last_processed_block_number − this.last_picked_block_number.
    /// (Pure channel occupancy: blocks forwarded by upstream not yet dequeued by this component.)
    pub component_block_diff_to_upstream: Family<ComponentId, Gauge<u64>>,
    /// Block-timestamp lag in seconds between this component and its upstream neighbour.
    /// Computed as upstream.last_processed_block_timestamp − this.last_picked_block_timestamp.
    /// (Channel occupancy in time units.) 0 if either timestamp is unavailable.
    pub component_time_diff_to_upstream_seconds: Family<ComponentId, Gauge<f64>>,
    /// Batches queued between this component and its upstream neighbour.
    /// Computed as upstream.batch_number − this.last_batch_picked.
    /// Only non-zero for batch-pipeline components with batch tracking.
    pub component_batch_diff_to_upstream: Family<ComponentId, Gauge<u64>>,
    /// Last batch number fully processed by this component (high-watermark).
    /// Only set for batch-pipeline components that call record_batch_number().
    pub component_last_processed_batch: Family<ComponentId, Gauge<u64>>,
    /// Last batch number dequeued from the input channel by this component (high-watermark).
    /// Only set for batch-pipeline components that call record_batch_picked():
    /// FriJobManager, SnarkJobManager, GaplessCommitter, GaplessL1ProofSender.
    pub component_last_picked_batch: Family<ComponentId, Gauge<u64>>,
    /// Oldest batch number currently assigned to an external prover (in-flight).
    /// Only set for FriJobManager and SnarkJobManager; 0 when the prover queue is empty.
    pub in_flight_first_batch: Family<ComponentId, Gauge<u64>>,
    /// Newest batch number currently assigned to an external prover (in-flight).
    /// Only set for FriJobManager and SnarkJobManager; 0 when the prover queue is empty.
    pub in_flight_last_batch: Family<ComponentId, Gauge<u64>>,
    /// Number of batches currently in-flight (in_flight_last − in_flight_first + 1).
    /// Only set for FriJobManager and SnarkJobManager; 0 when the prover queue is empty.
    pub in_flight_batch_count: Family<ComponentId, Gauge<u64>>,
    /// Counts transitions from Accepting to NotAccepting (transaction acceptance suspended).
    /// vise appends _total automatically for Counter types.
    pub acceptance_state_changes: Counter<u64>,
    /// Counts transitions from NotAccepting to Accepting (backpressure cleared).
    /// Paired with acceptance_state_changes so operators can track both sides.
    pub acceptance_state_clears: Counter<u64>,
    /// 1 if the monitor is currently accepting transactions, 0 if backpressure is active.
    pub accepting: Gauge<u64>,
}

#[vise::register]
pub static MONITOR_METRICS: vise::Global<MonitorMetrics> = vise::Global::new();
