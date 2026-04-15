use alloy::primitives::bytes::BytesMut;
use alloy::primitives::{Address, B256, BlockNumber};
use reth_network::Direction;
use reth_network_peers::PeerId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum ProtocolEvent {
    /// Connection established.
    Established {
        /// Connection direction.
        direction: Direction,
        /// Peer ID.
        peer_id: PeerId,
        /// Remote socket address observed when establishing the connection.
        remote_addr: SocketAddr,
    },
    /// Connection closed.
    Closed {
        /// Peer ID.
        peer_id: PeerId,
    },
    /// Peer requested replay stream starting from a specific block.
    ReplayRequested {
        /// Peer ID.
        peer_id: PeerId,
        /// First block peer expects to receive.
        starting_block: BlockNumber,
    },
    /// Peer requested verifier role for this session.
    VerifierRoleRequested {
        /// Peer ID.
        peer_id: PeerId,
    },
    /// Main node sent verifier challenge to peer.
    VerifierChallengeSent {
        /// Peer ID.
        peer_id: PeerId,
        /// Challenge nonce.
        nonce: B256,
    },
    /// Peer proved control of an accepted verifier signer.
    VerifierAuthorized {
        /// Peer ID.
        peer_id: PeerId,
        /// Recovered verifier signer.
        signer: Address,
    },
    /// Peer failed verifier authorization.
    VerifierUnauthorized {
        /// Peer ID.
        peer_id: PeerId,
        /// Recovered signer if signature parsing succeeded.
        signer: Option<Address>,
    },
    /// Replay record for a specific block was sent to peer.
    ReplayBlockSent {
        /// Peer ID.
        peer_id: PeerId,
        /// Block number contained in the replay record.
        block_number: BlockNumber,
    },
    /// Number of max active connections exceeded. New connection was rejected.
    MaxActiveConnectionsExceeded {
        /// The max number of active connections.
        max_connections: usize,
    },
}

/// Handle for sending messages to a currently connected peer.
#[derive(Debug, Clone)]
pub struct PeerConnectionHandle {
    /// Negotiated `zks` protocol version for this live connection.
    pub version: crate::version::ZksVersion,
    /// Channel used to queue encoded protocol frames to the peer.
    pub outbound_tx: mpsc::Sender<BytesMut>,
}

/// Registry of currently connected peers and their live protocol send handles.
pub(crate) type ConnectionRegistry = Arc<RwLock<HashMap<PeerId, PeerConnectionHandle>>>;
