//! The node's durable wire formats, in one place.
//!
//! Everything in this crate is bytes that outlive a single process or cross a trust
//! boundary: the versioned replay-record encodings (written to the write-ahead log,
//! streamed to external nodes, hashed into consensus block identities) and the
//! consensus-side envelopes built on them. Transient peer-to-peer messages — protocol
//! requests, votes, gossip framing — live with their protocols; this crate is only
//! for encodings that must stay decodable forever.
//!
//! The rules of the crate:
//!
//! - **Released encodings are immutable.** A `vN` module is never edited once
//!   released; changes add a `vN+1`. The golden tests under `tests/` pin every
//!   released encoding byte-for-byte.
//! - **Leaf dependencies only.** Both the networking stack and consensus depend on
//!   this crate; it depends on neither.

pub mod primitives;
pub use primitives::{BlockHashes, ForcedPreimage};

pub mod replays;

#[cfg(feature = "consensus")]
mod consensus_block;
#[cfg(feature = "consensus")]
pub use consensus_block::ConsensusBlock;

#[cfg(feature = "consensus")]
mod finality_certificate;
#[cfg(feature = "consensus")]
pub use finality_certificate::{FinalityCertificate, SignatureScheme};

#[cfg(feature = "consensus")]
mod epoch_transition;
#[cfg(feature = "consensus")]
pub use epoch_transition::{CommitteeMemberKeys, EpochTransition};
