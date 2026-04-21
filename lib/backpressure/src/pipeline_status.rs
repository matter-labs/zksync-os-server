use crate::ComponentId;
use std::sync::Arc;
use tokio::sync::watch;
use zksync_os_observability::ComponentState;
use zksync_os_types::TransactionAcceptanceState;

/// Outputs of setting up the backpressure monitoring system.
/// Returned by [`crate::BackpressureMonitor::spawn`], then consumed by the status server
/// and transaction acceptance gate.
#[derive(Clone)]
pub struct PipelineStatus {
    pub acceptance_rx: watch::Receiver<TransactionAcceptanceState>,
    pub component_states: Arc<Vec<(ComponentId, watch::Receiver<ComponentState>)>>,
    pub edges: Arc<Vec<(ComponentId, ComponentId)>>,
}
