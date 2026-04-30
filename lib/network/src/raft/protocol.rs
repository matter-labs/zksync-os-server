use crate::raft::wire::{RaftRequest, RaftResponse, RaftWireMessage, RequestId};
use async_trait::async_trait;
use dashmap::DashMap;
use futures::StreamExt;
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_eth_wire::protocol::Protocol;
use reth_network::Direction;
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network::types::Capability;
use reth_network_peers::PeerId;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, Instant, sleep};
use tracing::Instrument;

pub const RAFT_PROTOCOL: &str = "zks_raft";
const RAFT_PROTOCOL_VERSION: usize = 1;
// RLPx multiplexing uses the first byte as a sub-protocol message id.
// Raft has two wire message kinds (request/response), so it needs 2 slots.
const RAFT_PROTOCOL_MESSAGE_COUNT: u8 = 2;
const RAFT_OUTBOUND_CHANNEL_CAPACITY: usize = 64;

#[derive(Debug)]
struct PendingRequest {
    connection_id: u64,
    response_tx: oneshot::Sender<Result<RaftResponse, String>>,
}

#[async_trait]
pub trait RaftRequestHandler: Send + Sync + 'static {
    async fn handle(&self, request: RaftRequest) -> Result<RaftResponse, String>;
}

#[derive(Debug, Clone)]
pub struct RaftRouter {
    next_request_id: Arc<AtomicU64>,
    next_connection_id: Arc<AtomicU64>,
    // Stores (connection_id, response_sender) so that when a connection drops we can cancel
    // all requests that were routed through it.
    pending: Arc<DashMap<RequestId, PendingRequest>>,
    // Vec<PeerChannel> rather than a single PeerChannel because devp2p can establish two
    // simultaneous TCP connections for the same peer when both nodes dial each other at the same
    // time (each node sees an incoming connection from the other while its own outgoing connection
    // is also completing). devp2p closes the unwanted duplicate shortly after, but there is a
    // brief window where both connections are alive. Storing all of them means whichever one
    // devp2p decides to keep stays in the router after the other is removed by its Drop calling
    // unregister_peer. With a single slot, one connection would be silently orphaned and the peer
    // would appear disconnected even though the kept connection is alive.
    peers: Arc<DashMap<PeerId, Vec<PeerChannel>>>,
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
        self.peers.entry(peer_id).or_default().push(PeerChannel {
            connection_id,
            sender,
        });
        tracing::info!(%peer_id, connection_id, "raft peer connection registered");
        connection_id
    }

    pub fn unregister_peer(&self, peer_id: &PeerId, connection_id: u64) {
        let mut entry = match self.peers.entry(*peer_id) {
            dashmap::mapref::entry::Entry::Occupied(e) => e,
            dashmap::mapref::entry::Entry::Vacant(_) => return,
        };
        let channels = entry.get_mut();
        let before = channels.len();
        channels.retain(|ch| ch.connection_id != connection_id);
        if channels.len() == before {
            // connection_id was not in the list; already removed or never stored
            return;
        }
        if channels.is_empty() {
            entry.remove();
            tracing::info!(%peer_id, connection_id, "raft peer unregistered (no remaining connections)");
        } else {
            tracing::debug!(%peer_id, connection_id, remaining = channels.len(), "raft connection unregistered, peer still has other connections");
        }
    }

    pub fn send_request(
        &self,
        peer_id: PeerId,
        req: RaftRequest,
    ) -> Result<oneshot::Receiver<Result<RaftResponse, String>>, RaftTransportError> {
        let Some(channels) = self.peers.get(&peer_id) else {
            tracing::debug!(%peer_id, connected = self.peers.len(), "raft request failed: peer not connected");
            return Err(RaftTransportError::NotConnected(peer_id));
        };
        let senders = channels.value().clone();
        drop(channels);

        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        let mut msg = RaftWireMessage::Request { id, req };
        for ch in &senders {
            match ch.sender.send(msg) {
                Ok(()) => {
                    self.pending.insert(
                        id,
                        PendingRequest {
                            connection_id: ch.connection_id,
                            response_tx: tx,
                        },
                    );
                    return Ok(rx);
                }
                Err(tokio::sync::mpsc::error::SendError(returned)) => {
                    // This channel is dead (receiver dropped); its Drop will call unregister_peer.
                    // Recover the message and try the next connection.
                    tracing::debug!(%peer_id, connection_id = ch.connection_id, "raft send failed on connection, trying next");
                    msg = returned;
                }
            }
        }

        tracing::debug!(%peer_id, request_id = id, "raft request failed: all connections dead");
        Err(RaftTransportError::SendFailed(peer_id))
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
        if let Some((_, entry)) = self.pending.remove(&id) {
            let _ = entry.response_tx.send(resp);
        }
    }

    fn cancel_pending_for_connection(&self, connection_id: u64) {
        let matching: Vec<RequestId> = self
            .pending
            .iter()
            .filter(|e| e.value().connection_id == connection_id)
            .map(|e| *e.key())
            .collect();
        for id in matching {
            if let Some((_, entry)) = self.pending.remove(&id) {
                let _ = entry
                    .response_tx
                    .send(Err(format!("connection {connection_id} dropped")));
            }
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

    fn establish_connection(&self, peer_id: PeerId, conn: ProtocolConnection) -> RaftConnection {
        let (outbound_tx, outbound_rx) = mpsc::channel(RAFT_OUTBOUND_CHANNEL_CAPACITY);
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let connection_id = self.router.register_peer(peer_id, msg_tx);
        let task = tokio::spawn(
            run_raft_connection(
                peer_id,
                connection_id,
                conn,
                msg_rx,
                outbound_tx,
                self.handler.clone(),
                self.router.clone(),
            )
            .instrument(tracing::info_span!("raft_connection", %peer_id, connection_id)),
        );
        RaftConnection {
            peer_id,
            connection_id,
            router: self.router.clone(),
            outbound_rx,
            task,
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
        Protocol::new(
            Capability::new_static(RAFT_PROTOCOL, RAFT_PROTOCOL_VERSION),
            RAFT_PROTOCOL_MESSAGE_COUNT,
        )
    }

    fn on_unsupported_by_peer(
        self,
        _supported: &reth_eth_wire::capability::SharedCapabilities,
        _direction: Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        // Raft is an optional sub-protocol; non-raft peers should still be allowed to connect.
        OnNotSupported::KeepAlive
    }

    fn into_connection(
        self,
        direction: Direction,
        peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        tracing::info!(
            "raft sub-protocol connection established (direction={direction:?}, peer_id={peer_id})"
        );
        self.handler.establish_connection(peer_id, conn)
    }
}

/// Outbound side of a raft sub-protocol connection.
///
/// Wraps an mpsc receiver fed by a background Tokio task (`run_raft_connection`) that owns the
/// connection logic. Dropping this struct aborts the background task and cancels any pending
/// requests that were routed through this connection.
pub struct RaftConnection {
    peer_id: PeerId,
    connection_id: u64,
    router: RaftRouter,
    outbound_rx: mpsc::Receiver<alloy::primitives::bytes::BytesMut>,
    task: tokio::task::JoinHandle<()>,
}

impl futures::Stream for RaftConnection {
    type Item = alloy::primitives::bytes::BytesMut;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.outbound_rx.poll_recv(cx)
    }
}

impl Drop for RaftConnection {
    fn drop(&mut self) {
        tracing::info!(
            "raft connection dropped (peer_id={}, connection_id={}, pending_requests={})",
            self.peer_id,
            self.connection_id,
            self.router.pending.len(),
        );
        self.task.abort();
        self.router
            .cancel_pending_for_connection(self.connection_id);
        self.router
            .unregister_peer(&self.peer_id, self.connection_id);
    }
}

async fn run_raft_connection(
    peer_id: PeerId,
    connection_id: u64,
    mut conn: ProtocolConnection,
    mut msg_rx: mpsc::UnboundedReceiver<RaftWireMessage>,
    outbound_tx: mpsc::Sender<alloy::primitives::bytes::BytesMut>,
    handler: Arc<dyn RaftRequestHandler>,
    router: RaftRouter,
) {
    loop {
        tokio::select! {
            frame = conn.next() => {
                let Some(bytes) = frame else {
                    tracing::info!(%peer_id, connection_id, "raft connection closed by peer");
                    break;
                };
                match RaftWireMessage::decode(&bytes[..]) {
                    Ok(RaftWireMessage::Request { id, req }) => {
                        tracing::debug!(%peer_id, request_id = id, "received raft request");
                        let handler = handler.clone();
                        let outbound_tx = outbound_tx.clone();
                        tokio::spawn(async move {
                            let resp = handler.handle(req).await;
                            let encoded = RaftWireMessage::Response { id, resp };
                            let buf = alloy::primitives::bytes::BytesMut::from(
                                encoded.encode().as_slice(),
                            );
                            let _ = outbound_tx.send(buf).await;
                        });
                    }
                    Ok(RaftWireMessage::Response { id, resp }) => {
                        tracing::debug!(%peer_id, request_id = id, "received raft response");
                        router.complete_response(id, resp);
                    }
                    Err(error) => {
                        let preview_len = bytes.len().min(64);
                        let preview_hex = bytes[..preview_len]
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>();
                        tracing::warn!(
                            %peer_id,
                            connection_id,
                            %error,
                            msg_len = bytes.len(),
                            msg_preview_hex = %preview_hex,
                            "error decoding raft message; ignoring"
                        );
                    }
                }
            }
            msg = msg_rx.recv() => {
                let Some(msg) = msg else {
                    break;
                };
                let buf = alloy::primitives::bytes::BytesMut::from(msg.encode().as_slice());
                if outbound_tx.send(buf).await.is_err() {
                    break;
                }
            }
        }
    }
}
