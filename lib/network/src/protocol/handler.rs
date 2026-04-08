use super::config::{ExternalNodeProtocolConfig, MainNodeProtocolConfig};
use super::connection::ZksConnection;
use super::en::run_en_connection;
use super::events::PeerConnectionHandle;
use super::handler_shared_state::HandlerSharedState;
use super::mn::run_mn_connection;
use super::{ConnectionRegistry, ProtocolEvent};
use crate::version::ZksProtocolVersionSpec;
use crate::wire::message::{ZKS_PROTOCOL, ZksMessage};
use futures::{Stream, StreamExt};
use reth_eth_wire::capability::SharedCapabilities;
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_eth_wire::protocol::Protocol;
use reth_network::Direction;
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network_peers::PeerId;
use std::marker::PhantomData;
use std::net::SocketAddr;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tracing::Instrument;
use zksync_os_storage_api::ReadReplay;

/// Channel capacity for outbound protocol messages. Provides natural backpressure so the MN
/// does not produce records faster than the EN can consume them.
const OUTBOUND_CHANNEL_CAPACITY: usize = 32;

#[derive(Debug, Clone)]
enum ProtocolRole<Replay> {
    MainNode {
        replay: Replay,
        config: MainNodeProtocolConfig,
    },
    ExternalNode(ExternalNodeProtocolConfig),
}

#[derive(Debug, Clone)]
pub struct ZksProtocolHandler<P: ZksProtocolVersionSpec, Replay: Clone> {
    role: ProtocolRole<Replay>,
    /// Current state of the protocol.
    state: HandlerSharedState,
    connection_registry: ConnectionRegistry,
    _phantom: PhantomData<P>,
}

pub struct ZksProtocolConnectionHandler<P: ZksProtocolVersionSpec, Replay: Clone> {
    role: ProtocolRole<Replay>,
    /// Current state of the protocol.
    state: HandlerSharedState,
    connection_registry: ConnectionRegistry,
    remote_addr: SocketAddr,
    /// Owned permit that corresponds to a taken active connection slot.
    permit: OwnedSemaphorePermit,
    _phantom: PhantomData<P>,
}

impl<P: ZksProtocolVersionSpec, Replay: Clone> ZksProtocolHandler<P, Replay> {
    pub fn for_main_node(
        replay: Replay,
        config: MainNodeProtocolConfig,
        state: HandlerSharedState,
        connection_registry: ConnectionRegistry,
    ) -> Self {
        Self {
            role: ProtocolRole::MainNode { replay, config },
            state,
            connection_registry,
            _phantom: Default::default(),
        }
    }

    pub fn for_external_node(
        _replay: Replay,
        config: ExternalNodeProtocolConfig,
        state: HandlerSharedState,
        connection_registry: ConnectionRegistry,
    ) -> Self {
        Self {
            role: ProtocolRole::ExternalNode(config),
            state,
            connection_registry,
            _phantom: Default::default(),
        }
    }

    fn establish_connection(
        &self,
        remote_addr: SocketAddr,
        permit: OwnedSemaphorePermit,
    ) -> ZksProtocolConnectionHandler<P, Replay> {
        ZksProtocolConnectionHandler {
            role: self.role.clone(),
            state: self.state.clone(),
            connection_registry: self.connection_registry.clone(),
            remote_addr,
            permit,
            _phantom: Default::default(),
        }
    }

    fn try_establish_connection(
        &self,
        socket_addr: SocketAddr,
        peer_id: Option<PeerId>,
    ) -> Option<ZksProtocolConnectionHandler<P, Replay>> {
        match self.state.try_acquire_connection_slot() {
            Ok(permit) => Some(self.establish_connection(socket_addr, permit)),
            Err(_) => {
                match peer_id {
                    Some(peer_id) => tracing::warn!(
                        max_connections = self.state.max_active_connections(),
                        %socket_addr,
                        %peer_id,
                        "ignoring outgoing connection, max active reached"
                    ),
                    None => tracing::warn!(
                        max_connections = self.state.max_active_connections(),
                        %socket_addr,
                        "ignoring incoming connection, max active reached"
                    ),
                }
                self.state.emit_max_active_connections_exceeded();
                None
            }
        }
    }
}

impl<P: ZksProtocolVersionSpec, Replay: ReadReplay + Clone> ProtocolHandler
    for ZksProtocolHandler<P, Replay>
{
    type ConnectionHandler = ZksProtocolConnectionHandler<P, Replay>;

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

impl<P: ZksProtocolVersionSpec, Replay: ReadReplay + Clone> ConnectionHandler
    for ZksProtocolConnectionHandler<P, Replay>
{
    type Connection = ZksConnection;

    fn protocol(&self) -> Protocol {
        ZksMessage::<P>::protocol()
    }

    fn on_unsupported_by_peer(
        self,
        supported: &SharedCapabilities,
        _direction: Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        if supported.iter_caps().any(|c| c.name() == ZKS_PROTOCOL) {
            // Keep connection alive if there is at least one other common zks protocol version.
            OnNotSupported::KeepAlive
        } else {
            // Disconnect otherwise.
            OnNotSupported::Disconnect
        }
    }

    fn into_connection(
        self,
        direction: Direction,
        peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        let events_sender = self.state.events_sender();
        events_sender
            .send(ProtocolEvent::Established {
                direction,
                peer_id,
                remote_addr: self.remote_addr,
            })
            .ok();

        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
        self.connection_registry
            .write()
            .expect("protocol connection registry lock poisoned")
            .insert(
                peer_id,
                PeerConnectionHandle {
                    version: P::VERSION,
                    outbound_tx: outbound_tx.clone(),
                },
            );
        let conn = into_message_stream::<P>(conn);
        let connection_registry = self.connection_registry.clone();

        let task = match self.role {
            ProtocolRole::MainNode { replay, config } => tokio::spawn(
                run_mn_connection::<P, _>(
                    conn,
                    outbound_tx,
                    events_sender.clone(),
                    peer_id,
                    replay,
                    config,
                )
                .instrument(tracing::info_span!("mn_connection", %peer_id)),
            ),
            ProtocolRole::ExternalNode(config) => tokio::spawn(
                run_en_connection::<P>(conn, outbound_tx, peer_id, config)
                    .instrument(tracing::info_span!("en_connection", %peer_id)),
            ),
        };

        ZksConnection {
            outbound_rx,
            task,
            events_sender,
            peer_id,
            connection_registry,
            _permit: self.permit,
        }
    }
}

/// Wraps a raw `ProtocolConnection` into a typed message stream.
///
/// Each incoming byte frame is decoded as a `ZksMessage`. Decode errors are logged and terminate
/// the stream (by returning `None`), matching the behaviour of a closed connection.
fn into_message_stream<P: ZksProtocolVersionSpec>(
    conn: ProtocolConnection,
) -> impl Stream<Item = ZksMessage<P>> + Unpin + Send + 'static {
    Box::pin(conn.scan((), |_, raw| {
        let result = ZksMessage::<P>::decode_message(&mut &raw[..]);
        async move {
            match result {
                Ok(msg) => {
                    tracing::trace!(?msg, "processing peer message");
                    Some(msg)
                }
                Err(error) => {
                    tracing::info!(%error, "error decoding peer message; terminating");
                    None
                }
            }
        }
    }))
}
