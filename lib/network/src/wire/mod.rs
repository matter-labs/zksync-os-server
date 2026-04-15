//! Types for the ZKsync OS wire protocol aka zks (spec to be defined/written).

pub mod auth;
pub mod message;

pub mod primitives;
pub use primitives::{BlockHashes, ForcedPreimage};

pub mod replays;
pub use replays::{BlockReplays, GetBlockReplays};

pub mod verification;
