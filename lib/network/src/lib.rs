pub mod config;
pub(crate) mod metrics;
pub mod protocol;
pub mod service;
pub mod session;
pub mod twofa;
pub mod version;
mod wire;

// todo: temporary re-export while we have record overrides, otherwise `wire` module should be
//       entirely internal
pub use service::{NetworkPorts, PeerVerifyBatch, PeerVerifyBatchResult};
pub use twofa::{
    ExternalNode2faConfig, MainNode2faConfig, ZKS_2FA_PROTOCOL, Zks2faMessage,
    Zks2faProtocolHandler,
};
pub use wire::replays::RecordOverride;

/// Versioned wire encodings of replay records. Every version file is immutable once
/// released (new shapes get new version files), which makes these the only encodings of
/// a replay record that are safe to put on a network or hash into an identity.
///
/// Consumers today: the external-node replay sync protocol (in this crate) and the
/// consensus block encoding. The encodings do not depend on the networking machinery;
/// extracting them into a standalone crate is a known cleanup, at which point this
/// export disappears.
pub use wire::replays;
pub use wire::verification::{VerifyBatch, VerifyBatchOutcome, VerifyBatchResult};

// Re-export relevant Reth types
pub use reth_network::config::SecretKey;
pub use reth_network::config::rng_secret_key;
pub use reth_network_peers::NodeRecord;
pub use reth_network_peers::PeerId;
pub use reth_network_peers::TrustedPeer;
