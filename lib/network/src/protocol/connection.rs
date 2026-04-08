use super::{ConnectionRegistry, ProtocolEvent};
use alloy::primitives::bytes::BytesMut;
use reth_network_peers::PeerId;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::{OwnedSemaphorePermit, mpsc};

/// The outbound side of a zks protocol connection.
///
/// Wraps an mpsc receiver fed by a background Tokio task (`run_mn_connection()` or
/// `run_en_connection()`) that owns the actual protocol logic. Dropping this struct aborts the
/// background task, unregisters the peer, emits `ProtocolEvent::Closed`, and releases the
/// connection permit.
pub struct ZksConnection {
    pub(crate) outbound_rx: mpsc::Receiver<BytesMut>,
    pub(crate) task: tokio::task::JoinHandle<()>,
    pub(crate) events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    pub(crate) peer_id: PeerId,
    pub(crate) connection_registry: ConnectionRegistry,
    pub(crate) _permit: OwnedSemaphorePermit,
}

impl Drop for ZksConnection {
    fn drop(&mut self) {
        self.connection_registry
            .write()
            .expect("protocol connection registry lock poisoned")
            .remove(&self.peer_id);
        self.events_sender
            .send(ProtocolEvent::Closed {
                peer_id: self.peer_id,
            })
            .ok();
        self.task.abort();
    }
}

impl futures::Stream for ZksConnection {
    type Item = BytesMut;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.outbound_rx.poll_recv(cx)
    }
}
