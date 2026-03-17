//! An RLPX subprotocol for ZKsync OS functionality.

use crate::version::AnyZksProtocolVersion;
use crate::wire::message::{ZKS_PROTOCOL, ZksMessage};
use crate::wire::replays::{RecordOverride, WireReplayRecord};
use alloy::primitives::BlockNumber;
use alloy::primitives::bytes::BytesMut;
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
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::Instrument;
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

/// Channel capacity for outbound protocol messages. Provides natural backpressure so the MN
/// does not produce records faster than the EN can consume them.
const OUTBOUND_CHANNEL_CAPACITY: usize = 32;

impl<P: AnyZksProtocolVersion, Replay: ReadReplay + Clone> ConnectionHandler
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
        self.state
            .events_sender
            .send(ProtocolEvent::Established { direction, peer_id })
            .ok();

        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
        let conn = into_message_stream::<P>(conn);

        let task = if self.node_role.is_main() {
            tokio::spawn(
                run_mn_connection::<P, _>(conn, outbound_tx, self.replay)
                    .instrument(tracing::info_span!("mn_connection", %peer_id)),
            )
        } else {
            tokio::spawn(
                run_en_connection::<P>(
                    conn,
                    outbound_tx,
                    self.starting_block,
                    self.record_overrides,
                    self.replay_sender,
                )
                .instrument(tracing::info_span!("en_connection", %peer_id)),
            )
        };

        ZksConnection {
            outbound_rx,
            task,
            _permit: self.permit,
        }
    }
}

/// The outbound side of a zks protocol connection.
///
/// Wraps an mpsc receiver fed by a background Tokio task ([`run_mn_connection`] or
/// [`run_en_connection`]) that owns the actual protocol logic. Dropping this struct aborts the
/// background task and releases the connection permit.
pub struct ZksConnection {
    outbound_rx: mpsc::Receiver<BytesMut>,
    task: tokio::task::JoinHandle<()>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for ZksConnection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Stream for ZksConnection {
    type Item = BytesMut;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.outbound_rx.poll_recv(cx)
    }
}

/// Wraps a raw [`ProtocolConnection`] into a typed message stream.
///
/// Each incoming byte frame is decoded as a [`ZksMessage`]. Decode errors are logged and
/// terminate the stream (by returning `None`), matching the behaviour of a closed connection.
fn into_message_stream<P: AnyZksProtocolVersion>(
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

/// Background task that drives a **main-node** side of a connection.
///
/// Waits for a [`GetBlockReplays`] request from the EN, then streams replay records from
/// storage to the EN indefinitely.
async fn run_mn_connection<P: AnyZksProtocolVersion, Replay: ReadReplay + Clone>(
    mut conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    replay: Replay,
) {
    // Receive the single GetBlockReplays request for this connection.
    let request = match conn.next().await {
        Some(ZksMessage::GetBlockReplays(request)) => request,
        Some(ZksMessage::BlockReplays(_)) => {
            tracing::info!("received unexpected block replay response; terminating");
            return;
        }
        None => return,
    };

    // Stream records to the EN indefinitely.
    let mut stream = replay
        .clone()
        .stream_from_forever(request.starting_block, HashMap::new());
    loop {
        tokio::select! {
            // Biased because first branch always leads to early return. Makes sense to check it
            // first.
            biased;

            msg = conn.next() => {
                // No messages are expected from the peer after GetBlockReplays.
                match msg {
                    Some(msg) => tracing::info!(?msg, "received unexpected message from peer; terminating"),
                    None => tracing::info!("peer connection closed; terminating"),
                }
                return;
            }
            record = stream.next() => {
                let Some(record) = record else {
                    // stream_from_forever only ends if storage closes.
                    tracing::info!("replay stream closed; terminating");
                    return;
                };
                let encoded = ZksMessage::<P>::block_replays(vec![record]).encoded();
                if outbound_tx.send(encoded).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Background task that drives an **external-node** side of a connection.
///
/// Sends a [`GetBlockReplays`] request immediately, then forwards each received
/// [`BlockReplays`] record to the local sequencer via `replay_sender` and advances
/// `starting_block`.
async fn run_en_connection<P: AnyZksProtocolVersion>(
    mut conn: impl Stream<Item = ZksMessage<P>> + Unpin,
    outbound_tx: mpsc::Sender<BytesMut>,
    starting_block: Arc<RwLock<BlockNumber>>,
    record_overrides: Vec<RecordOverride>,
    replay_sender: mpsc::Sender<ReplayRecord>,
) {
    let next_block = *starting_block.read().unwrap();
    tracing::info!(next_block, "requesting block replays from main node");
    let msg = ZksMessage::<P>::get_block_replays(next_block, record_overrides);
    if outbound_tx.send(msg.encoded()).await.is_err() {
        return;
    }

    while let Some(msg) = conn.next().await {
        let response = match msg {
            ZksMessage::GetBlockReplays(_) => {
                tracing::info!("ignoring request as local node is also waiting for records");
                continue;
            }
            ZksMessage::BlockReplays(response) => response,
        };
        // todo: logic below relies on there being one record per message
        //       we can (and should) adapt it to handle multiple records in the future
        assert_eq!(
            response.records.len(),
            1,
            "only 1 record per message is supported right now"
        );
        let record = response.records.into_iter().next().unwrap();
        let block_number = record.block_number();
        tracing::debug!(block_number, "received block replay");
        let record: ReplayRecord = match record.try_into() {
            Ok(record) => record,
            Err(error) => {
                tracing::info!(%error, "failed to recover replay block");
                break;
            }
        };

        let expected_next_block = *starting_block.read().unwrap();
        assert_eq!(block_number, expected_next_block);

        if replay_sender.send(record).await.is_err() {
            tracing::trace!("network replay channel is closed");
            break;
        }
        // Only advance after the record is successfully delivered, so a reconnect
        // does not skip a block if the channel send was the last thing to fail.
        *starting_block.write().unwrap() += 1;
    }
}
