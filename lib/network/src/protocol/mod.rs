//! Replay streaming over the `zks` RLPx subprotocol.
//!
//! Verifier authentication and batch verification live in [`crate::twofa`]. Both subprotocols
//! publish [`ProtocolEvent`]s here so the network service can join replay progress with verifier
//! state for the same peer.

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
pub use events::ProtocolEvent;
pub use handler::{ZksProtocolConnectionHandler, ZksProtocolHandler};
pub use handler_shared_state::HandlerSharedState;

/// Maximum number of replay records carried in a single `BlockReplays` message.
const MAX_BLOCKS_PER_MESSAGE: u64 = 64;
