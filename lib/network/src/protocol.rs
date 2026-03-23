//! An RLPX subprotocol for ZKsync OS functionality.

use crate::version::AnyZksProtocolVersion;
use crate::wire::message::{ZKS_PROTOCOL, ZksMessage};
use crate::wire::replays::{RecordOverride, WireReplayRecord};
use crate::raft::wire::RaftWireMessage;
use alloy::primitives::BlockNumber;
use alloy::primitives::bytes::BytesMut;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, Stream, StreamExt};
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
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, ready};
use tokio::sync::mpsc::error::SendError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use zksync_os_storage_api::{ReadReplay, ReadReplayExt, ReplayRecord};
use zksync_os_types::NodeRole;

#[derive(Debug, Clone)]
pub struct ZksProtocolHandler<P: AnyZksProtocolVersion, Replay: Clone> {
    /// Storage to serve block replay records from.
    replay: Replay,
    /// Node's role in the network.
    node_role: NodeRole,
    /// Block number to start streaming from.
    starting_block: Arc<RwLock<BlockNumber>>,
    /// All overrides to pass through when requesting records.
    record_overrides: Vec<RecordOverride>,
    /// Current state of the protocol.
    state: ProtocolState,
    replay_sender: mpsc::Sender<ReplayRecord>,
    _phantom: PhantomData<P>,
}

impl<P: AnyZksProtocolVersion, Replay: Clone> ZksProtocolHandler<P, Replay> {
    pub fn new(
        replay: Replay,
        node_role: NodeRole,
        starting_block: Arc<RwLock<BlockNumber>>,
        record_overrides: Vec<RecordOverride>,
        state: ProtocolState,
        replay_sender: mpsc::Sender<ReplayRecord>,
    ) -> Self {
        Self {
            replay,
            node_role,
            starting_block,
            record_overrides,
            state,
            replay_sender,
            _phantom: Default::default(),
        }
    }

    fn establish_connection(
        &self,
        permit: OwnedSemaphorePermit,
    ) -> ZksProtocolConnectionHandler<P, Replay> {
        ZksProtocolConnectionHandler {
            replay: self.replay.clone(),
            node_role: self.node_role,
            starting_block: self.starting_block.clone(),
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
    starting_block: Arc<RwLock<BlockNumber>>,
    /// All overrides to pass through when requesting records.
    record_overrides: Vec<RecordOverride>,
    /// Current state of the protocol.
    state: ProtocolState,
    replay_sender: mpsc::Sender<ReplayRecord>,
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
                    starting_block: self.starting_block,
                    record_overrides: self.record_overrides,
                }
            },
            replay_sender: self.replay_sender.clone(),
            _permit: self.permit,
            _phantom: self._phantom,
        }
    }
}

pub struct ZksConnection<P: AnyZksProtocolVersion, Replay> {
    /// Remote peer ID.
    peer_id: PeerId,
    /// Protocol connection.
    conn: ProtocolConnection,
    /// Current connection state.
    state: State<Replay>,
    replay_sender: mpsc::Sender<ReplayRecord>,
    /// Owned permit that corresponds to a taken active connection slot.
    _permit: OwnedSemaphorePermit,
    _phantom: PhantomData<P>,
}

enum State<Replay> {
    // EN states
    /// Wants to send peer the request for streaming replay records.
    WantsToRequest {
        /// Starting block that the node will request streaming records from.
        starting_block: Arc<RwLock<BlockNumber>>,
        /// All overrides to pass through when requesting records.
        record_overrides: Vec<RecordOverride>,
    },
    /// Waits for peer to send replay records.
    WaitingForRecords {
        /// Next block that is expected to be sent by main node.
        next_block: Arc<RwLock<BlockNumber>>,
        /// Optional [`Future`] that is sending last received replay record.
        fut: Option<BoxFuture<'static, Result<(), SendError<ReplayRecord>>>>,
    },

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
        if let State::WantsToRequest {
            starting_block,
            record_overrides,
        } = &mut this.state
        {
            let next_block = *starting_block.read().unwrap();
            tracing::info!(next_block, "requesting block replays from main node");
            let message =
                ZksMessage::<P>::get_block_replays(next_block, std::mem::take(record_overrides));
            let encoded = message.encoded();
            this.state = State::WaitingForRecords {
                next_block: starting_block.clone(),
                fut: None,
            };
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
            // Make sure we do not have in-progress Future before trying to receive the next message
            if let State::WaitingForRecords {
                next_block,
                fut: Some(fut),
            } = &mut this.state
            {
                if ready!(fut.poll_unpin(cx)).is_err() {
                    tracing::trace!("network replay channel is closed");
                    break;
                }
                // Future completed, we can mark this record as sent
                *next_block.write().unwrap() += 1;
                this.state = State::WaitingForRecords {
                    next_block: next_block.clone(),
                    fut: None,
                };
            }
            let maybe_msg = ready!(this.conn.poll_next_unpin(cx));
            let Some(next) = maybe_msg else { break };
            let msg = match ZksMessage::<P>::decode_message(&mut &next[..]) {
                Ok(msg) => {
                    tracing::trace!(?msg, "processing peer message");
                    msg
                }
                Err(error) => {
                    let preview_len = next.len().min(64);
                    let preview_hex = next[..preview_len]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>();
                    let raft_decode = RaftWireMessage::decode(&next[..]).ok();
                    tracing::warn!(
                        %error,
                        msg_len = next.len(),
                        msg_preview_len = preview_len,
                        msg_preview_hex = %preview_hex,
                        looks_like_raft = raft_decode.is_some(),
                        "error decoding peer message; ignoring message"
                    );
                    if let Some(raft_msg) = raft_decode {
                        tracing::debug!(?raft_msg, "non-zks payload decodes as raft message");
                    }
                    continue;
                }
            };

            match msg {
                ZksMessage::GetBlockReplays(message) => {
                    // We take ownership of `state` by replacing it with `Terminated`. This is correct
                    // as long as all match branches below either evaluate into a new state or break
                    // with intention of terminating the connection.
                    this.state = match std::mem::replace(&mut this.state, State::Terminated) {
                        state @ State::WantsToRequest { .. } => {
                            tracing::info!(
                                "ignoring request as local node also wants to request records"
                            );
                            state
                        }
                        state @ State::WaitingForRecords { .. } => {
                            tracing::info!(
                                "ignoring request as local node is also waiting for records"
                            );
                            state
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
                    let next_block = match std::mem::replace(&mut this.state, State::Terminated) {
                        State::WaitingForRecords { fut, next_block } => {
                            if fut.is_some() {
                                unreachable!("we should not have in-progress future at this point");
                            }
                            // We are waiting for records and there is no in-progress future as expected,
                            // proceed with the rest of the logic
                            next_block
                        }
                        _ => {
                            tracing::info!("unrequested replay record received; terminating");
                            break;
                        }
                    };
                    // todo: logic below relies on there being one record per message
                    //       we can (and should) adapt it to handle multiple records in the future
                    assert_eq!(
                        message.records.len(),
                        1,
                        "only 1 record per message is supported right now"
                    );
                    let record = message.records.into_iter().next().unwrap();
                    let block_number = record.block_number();
                    tracing::debug!(block_number, "received block replay");
                    let record = match record.try_into() {
                        Ok(record) => record,
                        Err(error) => {
                            tracing::info!(%error, "failed to recover replay block");
                            break;
                        }
                    };

                    let expected_next_block = *next_block.read().unwrap();
                    assert_eq!(block_number, expected_next_block);

                    let sender = this.replay_sender.clone();
                    let mut fut = async move { sender.send(record).await }.boxed();
                    match fut.poll_unpin(cx) {
                        Poll::Ready(Ok(())) => {
                            // Future succeeded immediately, no need to save it
                            tracing::trace!(block_number, "sent block replay immediately");

                            // Future completed, we can mark this record as sent
                            *next_block.write().unwrap() += 1;

                            this.state = State::WaitingForRecords {
                                next_block,
                                fut: None,
                            };
                        }
                        Poll::Ready(Err(_)) => {
                            tracing::trace!("network replay channel is closed");
                            break;
                        }
                        Poll::Pending => {
                            // Future is pending, saving it to poll later
                            tracing::debug!(block_number, "sending block replay (pending)");
                            // It's important we do not increment `next_block` here as the connection
                            // might get severed before the future manages to complete.
                            this.state = State::WaitingForRecords {
                                next_block,
                                fut: Some(fut),
                            };
                            return Poll::Pending;
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
