use super::events::ProtocolEvent;
use reth_network_peers::PeerId;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc};

#[derive(Debug, Clone)]
pub struct HandlerSharedState {
    /// Protocol event sender.
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    /// The maximum number of active connections.
    max_active_connections: usize,
    active_connections_semaphore: Arc<Semaphore>,
    /// Peers that bypass the connection cap, so a pinned serving node is never locked out by other
    /// peers filling the pool.
    trusted_peers: Arc<HashSet<PeerId>>,
}

impl HandlerSharedState {
    /// Create new protocol state.
    pub fn new(
        events_sender: mpsc::UnboundedSender<ProtocolEvent>,
        max_active_connections: usize,
        trusted_peers: HashSet<PeerId>,
    ) -> Self {
        Self {
            events_sender,
            max_active_connections,
            active_connections_semaphore: Arc::new(Semaphore::new(max_active_connections)),
            trusted_peers: Arc::new(trusted_peers),
        }
    }

    /// Returns the current number of active connections.
    pub fn active_connections(&self) -> u64 {
        (self.max_active_connections - self.active_connections_semaphore.available_permits()) as u64
    }

    /// Whether `peer_id` is a trusted peer that bypasses the connection cap.
    pub(crate) fn is_trusted(&self, peer_id: &PeerId) -> bool {
        self.trusted_peers.contains(peer_id)
    }

    pub(crate) fn try_acquire_connection_slot(
        &self,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.active_connections_semaphore
            .clone()
            .try_acquire_owned()
    }

    pub(crate) fn events_sender(&self) -> mpsc::UnboundedSender<ProtocolEvent> {
        self.events_sender.clone()
    }

    pub(crate) fn emit_max_active_connections_exceeded(&self) {
        let _ = self
            .events_sender
            .send(ProtocolEvent::MaxActiveConnectionsExceeded {
                max_connections: self.max_active_connections,
            });
    }

    pub(crate) fn max_active_connections(&self) -> usize {
        self.max_active_connections
    }
}
