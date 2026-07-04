//! The node-side execution environment for BFT consensus.
//!
//! Consensus orders blocks; this crate is what it orders *of*. It implements the
//! consensus stack's execution interface on top of the node's real machinery:
//!
//! - [`block::ConsensusBlock`]: the unit of agreement — a thin consensus envelope
//!   around the node's replay record.
//! - [`pending_state`]: speculative per-block state overlays for blocks that consensus
//!   is still deciding on, floating above the strictly-linear durable state backend.
//! - [`env::NodeExecutionEnv`]: verification (proposal validity rules plus re-execution
//!   through the production VM) and durability-paced commit of finalized blocks into
//!   the node's persistence pipeline.
//! - [`rules`]: the validity rules bounding what a leader may put in a proposal.
//!
//! The node composition root wires this crate together with block building and
//! validator networking.

pub mod block;
pub mod builder;
pub mod env;
pub mod metrics;
pub mod pending_state;
pub mod rules;

pub use block::ConsensusBlock;
pub use builder::{BuilderConfig, BuiltBlock, ConsensusBlockBuilder, ParentInfo};
pub use env::{ChainAnchor, CommittedPayload, NodeExecutionEnv, ProposalValidation};
pub use pending_state::{BranchOverrides, CommittedHead, Overlay, PendingState};
pub use rules::{LocalL1Inputs, ValidityConfig, Verdict};
