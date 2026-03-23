use crate::raft::wire::{RaftRequest, RaftResponse, RaftWireMessage, RequestId};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_eth_wire::protocol::Protocol;
use reth_network::Direction;
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network::types::Capability;
use reth_network_peers::PeerId;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant, sleep};
use dashmap::DashMap;

pub const RAFT_PROTOCOL: &str = "raft";
const RAFT_PROTOCOL_VERSION: usize = 1;
// RLPx multiplexing uses the first byte as a sub-protocol message id.
// Raft has two wire message kinds (request/response), so it needs 2 slots.
const RAFT_PROTOCOL_MESSAGE_COUNT: u8 = 2;

#[async_trait]
pub trait RaftRequestHandler: Send + Sync + 'static {
    async fn handle(&self, request: RaftRequest) -> Result<RaftResponse, String>;
}

#[derive(Debug, Clone)]
pub struct RaftRouter {
    next_request_id: Arc<AtomicU64>,
    next_connection_id: Arc<AtomicU64>,
    pending: Arc<DashMap<RequestId, oneshot::Sender<Result<RaftResponse, String>>>>,
    peers: Arc<DashMap<PeerId, PeerChannel>>,
}

#[derive(Debug, Clone)]
struct PeerChannel {
    connection_id: u64,
    sender: mpsc::UnboundedSender<RaftWireMessage>,
}

impl Default for RaftRouter {
    fn default() -> Self {
        Self {
            next_request_id: Arc::new(AtomicU64::new(1)),
            next_connection_id: Arc::new(AtomicU64::new(1)),
            pending: Arc::new(DashMap::new()),
            peers: Arc::new(DashMap::new()),
        }
    }
}

impl RaftRouter {
    pub fn register_peer(
        &self,
        peer_id: PeerId,
        sender: mpsc::UnboundedSender<RaftWireMessage>,
    ) -> u64 {
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        self.peers.insert(
            peer_id,
            PeerChannel {
                connection_id,
                sender,
            },
        );
        tracing::info!(%peer_id, connection_id, "raft peer registered");
        connection_id
    }

    pub fn unregister_peer(&self, peer_id: &PeerId, connection_id: u64) {
        let should_remove = self
            .peers
            .get(peer_id)
            .map(|entry| entry.connection_id == connection_id)
            .unwrap_or(false);
        if should_remove {
            self.peers.remove(peer_id);
            tracing::info!(%peer_id, connection_id, "raft peer unregistered");
        }
    }

    pub fn send_request(
        &self,
        peer_id: PeerId,
        req: RaftRequest,
    ) -> Result<oneshot::Receiver<Result<RaftResponse, String>>, RaftTransportError> {
        let Some(peer) = self.peers.get(&peer_id) else {
            tracing::debug!(%peer_id, connected = self.peers.len(), "raft request failed: peer not connected");
            return Err(RaftTransportError::NotConnected(peer_id));
        };
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        if peer
            .sender
            .send(RaftWireMessage::Request { id, req })
            .is_err()
        {
            self.pending.remove(&id);
            tracing::debug!(%peer_id, request_id = id, "raft request failed: send error");
            return Err(RaftTransportError::SendFailed(peer_id));
        }
        Ok(rx)
    }

    pub fn connected_peers(&self) -> Vec<PeerId> {
        self.peers.iter().map(|entry| *entry.key()).collect()
    }

    pub async fn wait_for_peers(
        &self,
        peers: &[PeerId],
        timeout: Duration,
    ) -> Result<(), Vec<PeerId>> {
        let deadline = Instant::now() + timeout;
        let mut last_progress_log = Instant::now();
        loop {
            let connected = self.connected_peers();
            let missing: Vec<_> = peers
                .iter()
                .copied()
                .filter(|peer| !connected.contains(peer))
                .collect();

            if missing.is_empty() {
                tracing::info!(connected = ?connected, "all required raft peers are connected");
                return Ok(());
            }
            if Instant::now() >= deadline {
                tracing::warn!(missing = ?missing, connected = ?connected, "timed out waiting for raft peers");
                return Err(missing);
            }
            if last_progress_log.elapsed() >= Duration::from_secs(2) {
                tracing::info!(missing = ?missing, connected = ?connected, "still waiting for raft peers");
                last_progress_log = Instant::now();
            }

            sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn complete_response(&self, id: RequestId, resp: Result<RaftResponse, String>) {
        if let Some((_id, sender)) = self.pending.remove(&id) {
            let _ = sender.send(resp);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RaftTransportError {
    #[error("peer {0} is not connected")]
    NotConnected(PeerId),
    #[error("failed to send request to peer {0}")]
    SendFailed(PeerId),
}

#[derive(Clone)]
pub struct RaftProtocolHandler {
    handler: Arc<dyn RaftRequestHandler>,
    router: RaftRouter,
}

impl Debug for RaftProtocolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RaftProtocolHandler")
            .finish_non_exhaustive()
    }
}

impl RaftProtocolHandler {
    pub fn new(handler: impl RaftRequestHandler, router: RaftRouter) -> Self {
        Self {
            handler: Arc::new(handler),
            router,
        }
    }

    fn establish_connection(&self, peer_id: PeerId) -> RaftConnection {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let connection_id = self.router.register_peer(peer_id, outbound_tx);
        RaftConnection {
            peer_id,
            connection_id,
            conn: None,
            handler: self.handler.clone(),
            router: self.router.clone(),
            outbound_rx,
            outbound_queue: VecDeque::new(),
            inflight: None,
            close_reason: None,
        }
    }

    pub fn router(&self) -> RaftRouter {
        self.router.clone()
    }
}

impl ProtocolHandler for RaftProtocolHandler {
    type ConnectionHandler = RaftConnectionHandler;

    fn on_incoming(&self, _socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> {
        tracing::debug!("incoming raft sub-protocol connection handler requested");
        Some(RaftConnectionHandler {
            handler: self.clone(),
        })
    }

    fn on_outgoing(
        &self,
        _socket_addr: SocketAddr,
        _peer_id: PeerId,
    ) -> Option<Self::ConnectionHandler> {
        tracing::debug!("outgoing raft sub-protocol connection handler requested");
        Some(RaftConnectionHandler {
            handler: self.clone(),
        })
    }
}

pub struct RaftConnectionHandler {
    handler: RaftProtocolHandler,
}

impl ConnectionHandler for RaftConnectionHandler {
    type Connection = RaftConnection;

    fn protocol(&self) -> Protocol {
        Protocol::new(Capability::new_static(RAFT_PROTOCOL, RAFT_PROTOCOL_VERSION), RAFT_PROTOCOL_MESSAGE_COUNT)
    }

    fn on_unsupported_by_peer(
        self,
        supported: &reth_eth_wire::capability::SharedCapabilities,
        _direction: Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        if supported.iter_caps().any(|c| c.name() == RAFT_PROTOCOL) {
            OnNotSupported::KeepAlive
        } else {
            OnNotSupported::Disconnect
        }
    }

    fn into_connection(
        self,
        direction: Direction,
        peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        tracing::info!(?direction, %peer_id, "raft sub-protocol connection established");
        let mut connection = self.handler.establish_connection(peer_id);
        connection.conn = Some(conn);
        connection
    }
}

pub struct RaftConnection {
    peer_id: PeerId,
    connection_id: u64,
    conn: Option<ProtocolConnection>,
    handler: Arc<dyn RaftRequestHandler>,
    router: RaftRouter,
    outbound_rx: mpsc::UnboundedReceiver<RaftWireMessage>,
    outbound_queue: VecDeque<alloy::primitives::bytes::BytesMut>,
    inflight: Option<Pin<Box<dyn futures::Future<Output = Option<alloy::primitives::bytes::BytesMut>> + Send>>>,
    close_reason: Option<&'static str>,
}

impl Stream for RaftConnection {
    type Item = alloy::primitives::bytes::BytesMut;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(conn) = this.conn.as_mut() else {
            this.close_reason = Some("missing_protocol_connection");
            tracing::warn!(
                peer_id = %this.peer_id,
                connection_id = this.connection_id,
                "raft connection has no protocol connection; terminating stream"
            );
            return Poll::Ready(None);
        };

        if let Some(buf) = this.outbound_queue.pop_front() {
            return Poll::Ready(Some(buf));
        }

        if let Some(fut) = &mut this.inflight {
            if let Poll::Ready(Some(buf)) = fut.as_mut().poll(cx) {
                this.inflight = None;
                return Poll::Ready(Some(buf));
            }
        }

        while let Poll::Ready(Some(outbound)) = this.outbound_rx.poll_recv(cx) {
            this.outbound_queue
                .push_back(alloy::primitives::bytes::BytesMut::from(outbound.encode().as_slice()));
            if let Some(buf) = this.outbound_queue.pop_front() {
                return Poll::Ready(Some(buf));
            }
        }

        let maybe_msg = ready!(conn.poll_next_unpin(cx));
        let Some(next) = maybe_msg else {
            this.close_reason = Some("protocol_connection_closed_by_peer");
            tracing::info!(peer_id = %this.peer_id, connection_id = this.connection_id, "raft connection closed by peer");
            this.router.unregister_peer(&this.peer_id, this.connection_id);
            return Poll::Ready(None);
        };

        let msg = match RaftWireMessage::decode(&next[..]) {
            Ok(msg) => msg,
            Err(error) => {
                let preview_len = next.len().min(64);
                let preview_hex = next[..preview_len]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                tracing::warn!(
                    peer_id = %this.peer_id,
                    connection_id = this.connection_id,
                    %error,
                    msg_len = next.len(),
                    msg_preview_len = preview_len,
                    msg_preview_hex = %preview_hex,
                    "error decoding raft message; ignoring message"
                );
                return Poll::Pending;
            }
        };

        match msg {
            RaftWireMessage::Request { id, req } => {
                tracing::debug!(peer_id = %this.peer_id, request_id = id, "received raft request");
                let handler = this.handler.clone();
                let fut = async move {
                    let resp = handler.handle(req).await;
                    let encoded = RaftWireMessage::Response { id, resp };
                    Some(alloy::primitives::bytes::BytesMut::from(encoded.encode().as_slice()))
                };
                this.inflight = Some(Box::pin(fut));
                // Poll once right away so the future can register its waker. Otherwise this
                // stream may stall until unrelated network events happen.
                if let Some(fut) = &mut this.inflight {
                    if let Poll::Ready(Some(buf)) = fut.as_mut().poll(cx) {
                        this.inflight = None;
                        return Poll::Ready(Some(buf));
                    }
                }
                Poll::Pending
            }
            RaftWireMessage::Response { id, resp } => {
                tracing::debug!(peer_id = %this.peer_id, request_id = id, "received raft response");
                this.router.complete_response(id, resp);
                Poll::Pending
            }
        }
    }
}

impl Drop for RaftConnection {
    fn drop(&mut self) {
        tracing::debug!(
            peer_id = %self.peer_id,
            connection_id = self.connection_id,
            close_reason = self.close_reason.unwrap_or("dropped_by_owner"),
            pending_requests = self.router.pending.len(),
            "raft connection dropped"
        );
        self.router.unregister_peer(&self.peer_id, self.connection_id);
    }
}
