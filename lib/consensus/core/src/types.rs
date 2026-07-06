//! Concrete cryptography and consensus type choices for this network.
//!
//! Current assumptions, fixed for the first production rollout:
//!
//! - Validators are identified by ed25519 keys (also the p2p identity).
//! - Consensus votes and certificates use BLS12-381 **multi-signatures** (aggregated
//!   signature + signer bitmap). This needs no setup ceremony — unlike threshold
//!   signatures, which would require a distributed key generation among validators —
//!   and individual votes remain attributable, so misbehavior can be proven to third
//!   parties. The trade-off: certificate verification requires knowing the validator
//!   set (fine for validators and nodes that track the committee; a future move to
//!   threshold certificates would enable verification against a single static group
//!   key, e.g. for light clients, at the cost of running DKG ceremonies).
//! - Leaders rotate round-robin over the committee.
//! - The validator set is configured out-of-band as a committee *schedule*
//!   ([`crate::schedule`]): fixed within an epoch, changeable at epoch boundaries.

use commonware_consensus::simplex::elector::RoundRobin;
use commonware_consensus::simplex::scheme::bls12381_multisig;
use commonware_cryptography::Sha256;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::ed25519::PublicKey;

/// The certificate scheme instance for one validator in one epoch: its BLS key plus the
/// ordered committee. Non-validators use the verifier-only constructor of the same type.
pub type Scheme = bls12381_multisig::Scheme<PublicKey, MinPk>;

/// Supplies the scheme for a given epoch, derived from the committee schedule. Signer
/// for epochs where this validator is a member, verifier-only everywhere else — so
/// certificates from any historical epoch stay verifiable.
pub type SchemeProvider = crate::schedule::ScheduledSchemeProvider;

/// Deterministic round-robin leader rotation (hash-shuffled per epoch).
pub type Elector = RoundRobin<Sha256>;

/// The consensus activity stream over this network's concrete types — what an activity
/// reporter (metrics, status, fault evidence) receives.
pub type ConsensusActivity =
    commonware_consensus::simplex::types::Activity<Scheme, commonware_cryptography::sha256::Digest>;

/// Re-export for reporter implementations outside this crate.
pub use commonware_consensus::Reporter;
pub use commonware_consensus::simplex::types::{Activity, Attributable, Finalization};
pub use commonware_consensus::types::Epoch;
pub use commonware_parallel::Sequential;
