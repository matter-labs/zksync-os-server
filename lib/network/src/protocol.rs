//! An RLPX subprotocol for ZKsync OS functionality.

use crate::version::AnyZksProtocolVersion;
use crate::wire::message::{ZKS_PROTOCOL, ZksMessage};
use crate::wire::replays::WireReplayRecord;
use alloy::primitives::BlockNumber;
use alloy::primitives::bytes::BytesMut;
use futures::{Stream, StreamExt};
use reth_eth_wire::capability::SharedCapabilities;
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_eth_wire::protocol::Protocol;
use reth_network::Direction;
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network_peers::PeerId;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use zksync_os_storage_api::{ReadReplay, ReplayRecord};

#[derive(Debug, Clone)]
pub struct ZksProtocolHandler<P: AnyZksProtocolVersion, Replay: Clone> {
    /// Storage to serve block replay records from.
    pub replay: Replay,
    /// Whether this node wants to request blocks from its peers.
    pub to_request_blocks: bool,
    /// Current state of the protocol.
    pub state: ProtocolState,
    pub replay_sender: mpsc::UnboundedSender<ReplayRecord>,
    pub _phantom: PhantomData<P>,
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
            Ok(permit) => Some(ZksProtocolConnectionHandler {
                replay: self.replay.clone(),
                to_request_blocks: self.to_request_blocks,
                state: self.state.clone(),
                replay_sender: self.replay_sender.clone(),
                permit,
                _phantom: Default::default(),
            }),
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
            Ok(permit) => Some(ZksProtocolConnectionHandler {
                replay: self.replay.clone(),
                to_request_blocks: self.to_request_blocks,
                state: self.state.clone(),
                replay_sender: self.replay_sender.clone(),
                permit,
                _phantom: Default::default(),
            }),
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
    /// Whether this node wants to request blocks from its peers.
    to_request_blocks: bool,
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
            // todo: only request blocks if the peer is main node
            //       otherwise we can import incorrect blocks from diverged EN
            request_to_send: self.to_request_blocks.then(|| {
                // todo: support record_overrides
                ZksMessage::<P>::get_block_replays(self.replay.latest_record() + 1, vec![])
            }),
            response_state: None,
            replay: self.replay.clone(),
            replay_sender: self.replay_sender.clone(),
            terminated: false,
            _permit: self.permit,
        }
    }
}

pub struct ZksConnection<P: AnyZksProtocolVersion, Replay> {
    /// Peer ID.
    peer_id: PeerId,
    /// Protocol connection.
    conn: ProtocolConnection,
    request_to_send: Option<ZksMessage<P>>,
    response_state: Option<ResponseState>,
    replay: Replay,
    replay_sender: mpsc::UnboundedSender<ReplayRecord>,
    /// Flag indicating whether this stream has previously been terminated.
    terminated: bool,
    /// Owned permit that corresponds to a taken active connection slot.
    _permit: OwnedSemaphorePermit,
}

struct ResponseState {
    next_block_number: BlockNumber,
}

impl<P: AnyZksProtocolVersion, Replay: ReadReplay> Stream for ZksConnection<P, Replay> {
    type Item = BytesMut;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if this.terminated {
            return Poll::Ready(None);
        }

        let peer_id = this.peer_id;
        if let Some(request_to_send) = this.request_to_send.take() {
            return Poll::Ready(Some(request_to_send.encoded()));
        }

        let _span = tracing::info_span!("poll connection", %peer_id);
        loop {
            // todo: subscribe to new blocks
            if let Some(response_state) = &mut this.response_state
                && let Some(record) = this
                    .replay
                    .get_replay_record(response_state.next_block_number)
            {
                response_state.next_block_number += 1;
                return Poll::Ready(Some(ZksMessage::<P>::block_replays(vec![record]).encoded()));
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
                    if this.response_state.is_some() {
                        tracing::info!(
                            "received two `GetBlockReplays` requests from the same peer"
                        );
                        break;
                    }
                    this.response_state = Some(ResponseState {
                        next_block_number: message.starting_block,
                    });
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
        this.terminated = true;
        Poll::Ready(None)
    }
}
