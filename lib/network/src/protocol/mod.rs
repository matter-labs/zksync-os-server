//! An RLPX subprotocol for ZKsync OS functionality.

mod config;
mod connection;
mod en;
mod events;
mod handler;
mod handler_shared_state;
mod mn;

pub use config::{
    ExternalNodeProtocolConfig, ExternalNodeVerifierConfig, MainNodeProtocolConfig,
    ZksProtocolConfig,
};
pub use connection::ZksConnection;
pub(crate) use events::ConnectionRegistry;
pub use events::{PeerConnectionHandle, ProtocolEvent};
pub use handler::{ZksProtocolConnectionHandler, ZksProtocolHandler};
pub use handler_shared_state::HandlerSharedState;

/// Maximum number of replay records carried in a single `BlockReplays` message.
const MAX_BLOCKS_PER_MESSAGE: u64 = 64;
