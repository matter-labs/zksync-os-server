//! The `zks_2fa` RLPx subprotocol: verifier authentication and batch verification.
//!
//! Split out of the `zks` subprotocol so that replay streaming (`zks`) and verifier / batch
//! verification (`zks_2fa`) can evolve independently. Both subprotocols are multiplexed over the
//! same RLPx connection, so a verifier peer is correlated across them by its `PeerId`.

pub mod config;
mod en;
mod mn;
pub mod protocol;
pub mod wire;

pub use config::{ExternalNode2faConfig, MainNode2faConfig};
pub use protocol::{Zks2faConnectionRegistry, Zks2faPeerHandle, Zks2faProtocolHandler};
pub use wire::{ZKS_2FA_PROTOCOL, Zks2faMessage};
