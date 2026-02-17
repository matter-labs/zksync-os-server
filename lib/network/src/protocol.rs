//! An RLPX subprotocol for ZKsync OS functionality.

use crate::version::AnyZksProtocolVersion;
use crate::wire::message::{ZKS_PROTOCOL, ZksMessage};
use crate::wire::replays::{RecordOverride, WireReplayRecord};
use alloy::primitives::BlockNumber;
use alloy::primitives::bytes::BytesMut;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use reth_eth_wire::capability::SharedCapabilities;
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_eth_wire::protocol::Protocol;
use reth_network::Direction;
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network_peers::PeerId;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use zksync_os_storage_api::{ReadReplay, ReadReplayExt, ReplayRecord};
use zksync_os_types::NodeRole;

#[derive(Debug, Clone)]
pub struct ZksProtocolHandler<P: AnyZksProtocolVersion, Replay: Clone> {
    /// Storage to serve block replay records from.
    pub replay: Replay,
    /// Node's role in the network.
    pub node_role: NodeRole,
    /// Block number to start streaming from.
    pub starting_block: BlockNumber,
    /// All overrides to pass through when requesting records.
    pub record_overrides: Vec<RecordOverride>,
    /// Current state of the protocol.
    pub state: ProtocolState,
    pub replay_sender: mpsc::UnboundedSender<ReplayRecord>,
    pub _phantom: PhantomData<P>,
}

impl<P: AnyZksProtocolVersion, Replay: Clone> ZksProtocolHandler<P, Replay> {
    fn establish_connection(
        &self,
        permit: OwnedSemaphorePermit,
    ) -> ZksProtocolConnectionHandler<P, Replay> {
        ZksProtocolConnectionHandler {
            replay: self.replay.clone(),
            node_role: self.node_role,
            starting_block: self.starting_block,
            record_overrides: self.record_overrides.clone(),
            state: self.state.clone(),
            replay_sender: self.replay_sender.clone(),
            permit,
            _phantom: Default::default(),
        }
    }
}

impl<P: AnyZksProtocolVersion, Replay: ReadReplay + Clone> ProtocolHandler
    for ZksProtocolHandler<P, Replay>
{
    type ConnectionHandler = ZksProtocolConnectionHandler<P, Replay>;

    fn on_incoming(&self, socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> {
        match self
            .state
            .active_connections_semaphore
            .clone()
            .try_acquire_owned()
        {
            Ok(permit) => Some(self.establish_connection(permit)),
            Err(_) => {
                tracing::trace!(
                    max_connections = self.state.max_active_connections, %socket_addr,
                    "ignoring incoming connection, max active reached"
                );
                let _ =
                    self.state
                        .events_sender
                        .send(ProtocolEvent::MaxActiveConnectionsExceeded {
                            max_connections: self.state.max_active_connections,
                        });
                None
            }
        }
    }

    fn on_outgoing(
        &self,
        socket_addr: SocketAddr,
        peer_id: PeerId,
    ) -> Option<Self::ConnectionHandler> {
        match self
            .state
            .active_connections_semaphore
            .clone()
            .try_acquire_owned()
        {
            Ok(permit) => Some(self.establish_connection(permit)),
            Err(_) => {
                tracing::trace!(
                    max_connections = self.state.max_active_connections, %socket_addr, %peer_id,
                    "ignoring outgoing connection, max active reached"
                );
                let _ =
                    self.state
                        .events_sender
                        .send(ProtocolEvent::MaxActiveConnectionsExceeded {
                            max_connections: self.state.max_active_connections,
                        });
                None
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProtocolState {
    /// Protocol event sender.
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    /// The maximum number of active connections.
    max_active_connections: usize,
    active_connections_semaphore: Arc<Semaphore>,
}

impl ProtocolState {
    /// Create new protocol state.
    pub fn new(
        events_sender: mpsc::UnboundedSender<ProtocolEvent>,
        max_active_connections: usize,
    ) -> Self {
        Self {
            events_sender,
            max_active_connections,
            active_connections_semaphore: Arc::new(Semaphore::new(max_active_connections)),
        }
    }

    /// Returns the current number of active connections.
    pub fn active_connections(&self) -> u64 {
        (self.max_active_connections - self.active_connections_semaphore.available_permits()) as u64
    }
}

#[derive(Debug)]
pub enum ProtocolEvent {
    /// Connection established.
    Established {
        /// Connection direction.
        direction: Direction,
        /// Peer ID.
        peer_id: PeerId,
    },
    /// Number of max active connections exceeded. New connection was rejected.
    MaxActiveConnectionsExceeded {
        /// The max number of active connections.
        max_connections: usize,
    },
}

pub struct ZksProtocolConnectionHandler<P: AnyZksProtocolVersion, Replay: Clone> {
    /// Storage to serve block replay records from.
    replay: Replay,
    /// Node's role in the network.
    node_role: NodeRole,
    /// Block number to start streaming from.
    starting_block: BlockNumber,
    /// All overrides to pass through when requesting records.
    record_overrides: Vec<RecordOverride>,
    /// Current state of the protocol.
    state: ProtocolState,
    replay_sender: mpsc::UnboundedSender<ReplayRecord>,
    /// Owned permit that corresponds to a taken active connection slot.
    permit: OwnedSemaphorePermit,
    _phantom: PhantomData<P>,
}

impl<P: AnyZksProtocolVersion, Replay: ReadReplay + Clone> ConnectionHandler
    for ZksProtocolConnectionHandler<P, Replay>
{
    type Connection = ZksConnection<P, Replay>;

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
            // Keep connection alive if there is at least one other common zks protocol version
            OnNotSupported::KeepAlive
        } else {
            // Disconnect otherwise
            OnNotSupported::Disconnect
        }
    }

    fn into_connection(
        self,
        direction: Direction,
        peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        // Emit connection established event.
        self.state
            .events_sender
            .send(ProtocolEvent::Established { direction, peer_id })
            .ok();

        ZksConnection {
            peer_id,
            conn,
            state: if self.node_role.is_main() {
                State::WaitingForRequest {
                    replay: self.replay.clone(),
                }
            } else {
                State::WantsToRequest {
                    message: ZksMessage::<P>::get_block_replays(
                        self.starting_block,
                        self.record_overrides,
                    ),
                }
            },
            replay_sender: self.replay_sender.clone(),
            _permit: self.permit,
        }
    }
}

pub struct ZksConnection<P: AnyZksProtocolVersion, Replay> {
    /// Remote peer ID.
    peer_id: PeerId,
    /// Protocol connection.
    conn: ProtocolConnection,
    /// Current connection state.
    state: State<P, Replay>,
    replay_sender: mpsc::UnboundedSender<ReplayRecord>,
    /// Owned permit that corresponds to a taken active connection slot.
    _permit: OwnedSemaphorePermit,
}

enum State<P: AnyZksProtocolVersion, Replay> {
    // EN states
    /// Wants to send peer the request for streaming replay records.
    WantsToRequest { message: ZksMessage<P> },
    /// Waits for peer to send replay records.
    WaitingForRecords,

    // MN states
    /// Waits for peer to request streaming replay records.
    WaitingForRequest { replay: Replay },
    /// Currently streaming replay records.
    Responding {
        stream: BoxStream<'static, ReplayRecord>,
    },

    /// Indicates that this stream has previously been terminated.
    Terminated,
}

impl<P: AnyZksProtocolVersion, Replay: ReadReplay> Stream for ZksConnection<P, Replay> {
    type Item = BytesMut;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if matches!(this.state, State::Terminated) {
            return Poll::Ready(None);
        }

        let peer_id = this.peer_id;
        if let State::WantsToRequest { message } = &this.state {
            let encoded = message.encoded();
            this.state = State::WaitingForRecords;
            return Poll::Ready(Some(encoded));
        }

        let _span = tracing::info_span!("poll connection", %peer_id);
        loop {
            if let State::Responding { stream } = &mut this.state {
                match stream.poll_next_unpin(cx) {
                    Poll::Ready(Some(record)) => {
                        return Poll::Ready(Some(
                            ZksMessage::<P>::block_replays(vec![record]).encoded(),
                        ));
                    }
                    Poll::Ready(None) => {
                        tracing::info!("replay stream is closed; terminating connection");
                        break;
                    }
                    Poll::Pending => {}
                }
            }
            let maybe_msg = ready!(this.conn.poll_next_unpin(cx));
            let Some(next) = maybe_msg else { break };
            let msg = match ZksMessage::<P>::decode_message(&mut &next[..]) {
                Ok(msg) => {
                    tracing::trace!(?msg, "processing peer message");
                    msg
                }
                Err(error) => {
                    tracing::info!(%error, "error decoding peer message");
                    break;
                }
            };

            match msg {
                ZksMessage::GetBlockReplays(message) => {
                    // We take ownership of `state` by replacing it with `Terminated`. This is correct
                    // as long as all match branches below either evaluate into a new state or break
                    // with intention of terminating the connection.
                    this.state = match std::mem::replace(&mut this.state, State::Terminated) {
                        State::WantsToRequest { message } => {
                            tracing::info!(
                                "ignoring request as local node also wants to request records"
                            );
                            State::WantsToRequest { message }
                        }
                        State::WaitingForRecords => {
                            tracing::info!(
                                "ignoring request as local node is also waiting for records"
                            );
                            State::WaitingForRecords
                        }
                        State::WaitingForRequest { replay } => State::Responding {
                            stream: replay
                                .stream_from_forever(message.starting_block, HashMap::new()),
                        },
                        State::Responding { .. } => {
                            tracing::info!(
                                "received two `GetBlockReplays` requests from the same peer"
                            );
                            break;
                        }
                        State::Terminated => {
                            break;
                        }
                    };
                }
                ZksMessage::BlockReplays(message) => {
                    for record in message.records {
                        tracing::debug!(
                            block_number = record.block_number(),
                            "received block replay"
                        );
                        let record = match record.try_into() {
                            Ok(record) => record,
                            Err(error) => {
                                tracing::info!(%error, "failed to recover replay block");
                                break;
                            }
                        };
                        if this.replay_sender.send(record).is_err() {
                            tracing::trace!("network replay channel is closed");
                            break;
                        }
                    }
                }
            }
        }

        // Terminate the connection.
        this.state = State::Terminated;
        Poll::Ready(None)
    }
}
