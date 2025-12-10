//! An RLPX subprotocol for ZKsync OS functionality.

use crate::wire::GetBlockReplays;
use crate::wire::message::ZksMessage;
use alloy::primitives::BlockNumber;
use alloy::primitives::bytes::BytesMut;
use futures::{Stream, StreamExt};
use reth_eth_wire::capability::SharedCapabilities;
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_eth_wire::protocol::Protocol;
use reth_network::Direction;
use reth_network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_network_peers::PeerId;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use zksync_os_storage_api::ReadReplay;

#[derive(Debug, Clone)]
pub struct ZksProtocolHandler<Replay: Clone> {
    /// The maximum number of active connections.
    pub max_active_connections: u64,
    /// Storage to serve block replay records from.
    pub replay: Replay,
    /// Whether this node wants to request blocks from its peers.
    pub to_request_blocks: bool,
    /// Current state of the protocol.
    pub state: ProtocolState,
}

impl<Replay: ReadReplay + Clone> ProtocolHandler for ZksProtocolHandler<Replay> {
    type ConnectionHandler = Self;

    fn on_incoming(&self, socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> {
        let num_active = self.state.active_connections();
        if num_active >= self.max_active_connections {
            tracing::trace!(
                num_active, max_connections = self.max_active_connections, %socket_addr,
                "ignoring incoming connection, max active reached"
            );
            let _ = self
                .state
                .events_sender
                .send(ProtocolEvent::MaxActiveConnectionsExceeded { num_active });
            None
        } else {
            Some(self.clone())
        }
    }

    fn on_outgoing(
        &self,
        socket_addr: SocketAddr,
        peer_id: PeerId,
    ) -> Option<Self::ConnectionHandler> {
        let num_active = self.state.active_connections();
        if num_active >= self.max_active_connections {
            tracing::trace!(
                num_active, max_connections = self.max_active_connections, %socket_addr, %peer_id,
                "ignoring outgoing connection, max active reached"
            );
            let _ = self
                .state
                .events_sender
                .send(ProtocolEvent::MaxActiveConnectionsExceeded { num_active });
            None
        } else {
            Some(self.clone())
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProtocolState {
    /// Protocol event sender.
    events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    /// The number of active connections.
    active_connections: Arc<AtomicU64>,
}

impl ProtocolState {
    /// Create new protocol state.
    pub fn new(events_sender: mpsc::UnboundedSender<ProtocolEvent>) -> Self {
        Self {
            events_sender,
            active_connections: Arc::default(),
        }
    }

    /// Returns the current number of active connections.
    pub fn active_connections(&self) -> u64 {
        self.active_connections.load(Ordering::Relaxed)
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
        /// The current number of active connections.
        num_active: u64,
    },
}

impl<Replay: ReadReplay + Clone> ConnectionHandler for ZksProtocolHandler<Replay> {
    type Connection = ZksConnection<Replay>;

    fn protocol(&self) -> Protocol {
        ZksMessage::protocol()
    }

    fn on_unsupported_by_peer(
        self,
        _supported: &SharedCapabilities,
        _direction: Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        OnNotSupported::Disconnect
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

        // Increment the number of active sessions.
        self.state
            .active_connections
            .fetch_add(1, Ordering::Relaxed);

        ZksConnection {
            peer_id,
            conn,
            request_to_send: self.to_request_blocks.then(|| {
                ZksMessage::GetBlockReplays(GetBlockReplays {
                    starting_block: self.replay.latest_record() + 1,
                    // todo: populate with real values
                    record_overrides: vec![],
                })
            }),
            response_state: None,
            replay: self.replay.clone(),
            terminated: false,
        }
    }
}

pub struct ZksConnection<Replay> {
    /// Peer ID.
    peer_id: PeerId,
    /// Protocol connection.
    conn: ProtocolConnection,
    request_to_send: Option<ZksMessage>,
    response_state: Option<ResponseState>,
    replay: Replay,
    /// Flag indicating whether this stream has previously been terminated.
    terminated: bool,
}

struct ResponseState {
    next_block_number: BlockNumber,
    #[expect(dead_code)]
    request: GetBlockReplays,
}

impl<Replay: ReadReplay> Stream for ZksConnection<Replay> {
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

        loop {
            // todo: subscribe to new blocks
            if let Some(response_state) = &mut this.response_state
                && let Some(record) = this
                    .replay
                    .get_replay_record(response_state.next_block_number)
            {
                response_state.next_block_number += 1;
                return Poll::Ready(Some(ZksMessage::block_replays(vec![record]).encoded()));
            }
            if let Poll::Ready(maybe_msg) = this.conn.poll_next_unpin(cx) {
                let Some(next) = maybe_msg else { break };
                let msg = match ZksMessage::decode_message(&mut &next[..]) {
                    Ok(msg) => {
                        tracing::trace!(%peer_id, ?msg, "processing peer message");
                        msg
                    }
                    Err(error) => {
                        tracing::debug!(%peer_id, %error, "error decoding peer message");
                        break;
                    }
                };

                match msg {
                    ZksMessage::GetBlockReplays(message) => {
                        if this.response_state.is_some() {
                            tracing::trace!(%peer_id, "received two `GetBlockReplays` requests from the same peer");
                            break;
                        }
                        this.response_state = Some(ResponseState {
                            next_block_number: message.starting_block,
                            request: message,
                        });
                    }
                    ZksMessage::BlockReplays(message) => {
                        for record in message.records {
                            tracing::info!(
                                %peer_id, block_number = record.block_context.block_number,
                                "received block replay"
                            );
                            // todo: propagate new records to sequencer
                        }
                    }
                }
                continue;
            }

            return Poll::Pending;
        }

        // Terminate the connection.
        this.terminated = true;
        Poll::Ready(None)
    }
}
