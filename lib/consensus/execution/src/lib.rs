//! The node-side execution environment for BFT consensus.
//!
//! Consensus orders blocks; this crate is what it orders *of*. It implements the
//! consensus stack's execution interface on top of the node's real machinery:
//!
//! - [`block::ConsensusBlock`]: the unit of agreement — a thin consensus envelope
//!   around the node's replay record.
//! - [`pending_state`]: speculative per-block state overlays for blocks that consensus
//!   is still deciding on, floating above the strictly-linear durable state backend.
//! - [`env::NodeExecutionEnv`]: verification by re-execution through the production VM,
//!   and durability-paced commit of finalized blocks into the node's persistence
//!   pipeline.
//!
//! Nothing in this crate is wired into the running node yet; the node composition root
//! adopts it together with block building and validator networking.

pub mod block;
pub mod env;
pub mod pending_state;

pub use block::ConsensusBlock;
pub use env::{ChainAnchor, CommittedPayload, NodeExecutionEnv};
pub use pending_state::{BranchOverrides, CommittedHead, Overlay, PendingState};
