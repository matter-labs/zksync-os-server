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
//! - The validator set is static, configured out-of-band, and all heights currently
//!   live in one long epoch. The per-epoch machinery (schemes and engines are built
//!   per epoch) is already shaped for rotating committees later.

use commonware_consensus::simplex::elector::RoundRobin;
use commonware_consensus::simplex::scheme::bls12381_multisig;
use commonware_consensus::types::Epoch;
use commonware_cryptography::Sha256;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::certificate::ConstantProvider;
use commonware_cryptography::ed25519::PublicKey;

/// The certificate scheme instance for one validator in one epoch: its BLS key plus the
/// ordered committee. Non-validators use the verifier-only constructor of the same type.
pub type Scheme = bls12381_multisig::Scheme<PublicKey, MinPk>;

/// Supplies the scheme for a given epoch. With a static validator set there is exactly
/// one committee, so a constant provider suffices; a rotating-committee provider slots in
/// here when validator-set changes arrive.
pub type SchemeProvider = ConstantProvider<Scheme, Epoch>;

/// Deterministic round-robin leader rotation (hash-shuffled per epoch).
pub type Elector = RoundRobin<Sha256>;
