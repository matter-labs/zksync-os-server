use super::config::{ExternalNode2faConfig, MainNode2faConfig};
use super::en::run_2fa_en_connection;
use super::mn::run_2fa_mn_connection;
use super::wire::Zks2faMessage;
use crate::protocol::HandlerSharedState;
use alloy::primitives::bytes::BytesMut;
use futures::{Stream, StreamExt};
use reth_eth_wire::capability::SharedCapabilities;
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_eth_wire::protocol::Protocol;
use reth_network::Direction;
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network_peers::PeerId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tracing::Instrument;

/// Channel capacity for outbound `zks_2fa` protocol messages.
const OUTBOUND_CHANNEL_CAPACITY: usize = 32;

/// Handle for sending messages to a peer over its live `zks_2fa` connection.
#[derive(Debug, Clone)]
pub struct Zks2faPeerHandle {
    /// Channel used to queue encoded protocol frames to the peer.
    pub outbound_tx: mpsc::Sender<BytesMut>,
}

/// Registry of currently connected `zks_2fa` peers and their live outbound send handles.
pub type Zks2faConnectionRegistry = Arc<RwLock<HashMap<PeerId, Zks2faPeerHandle>>>;

#[derive(Debug, Clone)]
enum Twofa2Role {
    MainNode(MainNode2faConfig),
    ExternalNode(ExternalNode2faConfig),
}

#[derive(Debug, Clone)]
pub struct Zks2faProtocolHandler {
    role: Twofa2Role,
    state: HandlerSharedState,
    connection_registry: Zks2faConnectionRegistry,
}

pub struct Zks2faConnectionHandler {
    role: Twofa2Role,
    state: HandlerSharedState,
    connection_registry: Zks2faConnectionRegistry,
    /// Owned permit that corresponds to a taken active connection slot.
    permit: OwnedSemaphorePermit,
}

impl Zks2faProtocolHandler {
    pub fn for_main_node(
        config: MainNode2faConfig,
        state: HandlerSharedState,
        connection_registry: Zks2faConnectionRegistry,
    ) -> Self {
        Self {
            role: Twofa2Role::MainNode(config),
            state,
            connection_registry,
        }
    }

    pub fn for_external_node(
        config: ExternalNode2faConfig,
        state: HandlerSharedState,
        connection_registry: Zks2faConnectionRegistry,
    ) -> Self {
        Self {
            role: Twofa2Role::ExternalNode(config),
            state,
            connection_registry,
        }
    }

    fn try_establish_connection(
        &self,
        socket_addr: SocketAddr,
        peer_id: Option<PeerId>,
    ) -> Option<Zks2faConnectionHandler> {
        match self.state.try_acquire_connection_slot() {
            Ok(permit) => Some(Zks2faConnectionHandler {
                role: self.role.clone(),
                state: self.state.clone(),
                connection_registry: self.connection_registry.clone(),
                permit,
            }),
            Err(_) => {
                match peer_id {
                    Some(peer_id) => tracing::warn!(
                        max_connections = self.state.max_active_connections(),
                        %socket_addr,
                        %peer_id,
                        "ignoring outgoing zks_2fa connection, max active reached"
                    ),
                    None => tracing::warn!(
                        max_connections = self.state.max_active_connections(),
                        %socket_addr,
                        "ignoring incoming zks_2fa connection, max active reached"
                    ),
                }
                self.state.emit_max_active_connections_exceeded();
                None
            }
        }
    }
}

impl ProtocolHandler for Zks2faProtocolHandler {
    type ConnectionHandler = Zks2faConnectionHandler;

    fn on_incoming(&self, socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> {
        self.try_establish_connection(socket_addr, None)
    }

    fn on_outgoing(
        &self,
        socket_addr: SocketAddr,
        peer_id: PeerId,
    ) -> Option<Self::ConnectionHandler> {
        self.try_establish_connection(socket_addr, Some(peer_id))
    }
}

impl ConnectionHandler for Zks2faConnectionHandler {
    type Connection = Zks2faConnection;

    fn protocol(&self) -> Protocol {
        Zks2faMessage::protocol()
    }

    fn on_unsupported_by_peer(
        self,
        _supported: &SharedCapabilities,
        _direction: Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        // `zks_2fa` is an optional sub-protocol; replay-only peers must stay connected.
        OnNotSupported::KeepAlive
    }

    fn into_connection(
        self,
        _direction: Direction,
        peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        // Note: session lifecycle (`Established`/`Closed`) is intentionally owned by the `zks`
        // replay connection, which every verifier peer also has. Emitting those events here too
        // would double-count peers in `PeerSessionStore`. We only emit verifier-specific events.
        let events_sender = self.state.events_sender();
        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
        self.connection_registry
            .write()
            .expect("zks_2fa connection registry lock poisoned")
            .insert(
                peer_id,
                Zks2faPeerHandle {
                    outbound_tx: outbound_tx.clone(),
                },
            );
        let conn = into_message_stream(conn);
        let connection_registry = self.connection_registry.clone();

        let task = match self.role {
            Twofa2Role::MainNode(config) => tokio::spawn(
                run_2fa_mn_connection(conn, outbound_tx, events_sender, peer_id, config)
                    .instrument(tracing::info_span!("zks_2fa_mn_connection", %peer_id)),
            ),
            Twofa2Role::ExternalNode(config) => tokio::spawn(
                run_2fa_en_connection(conn, outbound_tx, peer_id, config)
                    .instrument(tracing::info_span!("zks_2fa_en_connection", %peer_id)),
            ),
        };

        Zks2faConnection {
            outbound_rx,
            task,
            peer_id,
            connection_registry,
            _permit: self.permit,
        }
    }
}

/// The outbound side of a `zks_2fa` protocol connection.
///
/// Wraps an mpsc receiver fed by a background Tokio task (`run_2fa_mn_connection` or
/// `run_2fa_en_connection`). Dropping this struct unregisters the peer from the connection registry
/// and aborts the background task.
pub struct Zks2faConnection {
    outbound_rx: mpsc::Receiver<BytesMut>,
    task: tokio::task::JoinHandle<()>,
    peer_id: PeerId,
    connection_registry: Zks2faConnectionRegistry,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Zks2faConnection {
    fn drop(&mut self) {
        self.connection_registry
            .write()
            .expect("zks_2fa connection registry lock poisoned")
            .remove(&self.peer_id);
        self.task.abort();
    }
}

impl Stream for Zks2faConnection {
    type Item = BytesMut;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.outbound_rx.poll_recv(cx)
    }
}

/// Wraps a raw `ProtocolConnection` into a typed `Zks2faMessage` stream.
///
/// Decode errors are logged and terminate the stream (by returning `None`), matching the behaviour
/// of a closed connection.
fn into_message_stream(
    conn: ProtocolConnection,
) -> impl Stream<Item = Zks2faMessage> + Unpin + Send + 'static {
    Box::pin(conn.scan((), |_, raw| {
        let result = Zks2faMessage::decode_message(&mut &raw[..]);
        async move {
            match result {
                Ok(msg) => {
                    tracing::trace!(?msg, "processing peer zks_2fa message");
                    Some(msg)
                }
                Err(error) => {
                    tracing::info!(%error, "error decoding peer zks_2fa message; terminating");
                    None
                }
            }
        }
    }))
}
