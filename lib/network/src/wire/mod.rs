//! Types for the ZKsync OS wire protocol aka zks (spec to be defined/written).
//!
//! Only the protocol *messages* live here. The durable encodings they carry — the
//! versioned replay-record formats and the primitive wire types — live in
//! `zksync_os_wire` (shared with consensus) and are re-exported through
//! [`replays`].

pub mod auth;
pub mod message;

pub mod replays;
pub use replays::{BlockReplays, GetBlockReplays};

pub mod verification;
