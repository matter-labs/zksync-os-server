//! BFT consensus for zksync-os-server, built on [commonware](https://commonware.xyz)
//! simplex.
//!
//! # What this crate is
//!
//! The consensus subsystem that lets a set of mutually-distrusting validators agree on
//! the sequence of blocks: leader rotation, voting, finality certificates, block
//! dissemination, backfill, and crash-safe restart. It is deliberately ignorant of what
//! a block *contains* — everything content-related happens behind the [`ExecutionEnv`]
//! trait, implemented by the node (and by mock/in-memory environments in tests).
//!
//! The one non-negotiable design rule: **this crate never depends on the node's
//! sequencer, state, or storage crates.** Consensus sees the node only through
//! [`ExecutionEnv`]. This is what allows the entire stack — engine, marshal, gossip,
//! backfill, storage — to run unmodified inside commonware's deterministic runtime,
//! where multi-validator scenarios (crashes, partitions, byzantine peers) replay
//! bit-exactly from a seed.
//!
//! # Layout
//!
//! - [`execution`]: the [`ExecutionEnv`] boundary trait.
//! - [`types`]: the concrete cryptography choices (BLS multisig over ed25519 identities,
//!   round-robin leaders).
//! - [`schedule`]: the committee schedule — which validator set holds which epochs —
//!   and the per-epoch scheme provider derived from it.
//! - [`registry`]: the on-chain-registry derivation driver — how chain state becomes
//!   per-epoch committee decisions (behind source/ledger traits; the node supplies
//!   real state and storage, simulations supply manufactured ones).
//! - [`application`] / [`committer`]: adapters between consensus and [`ExecutionEnv`].
//! - [`storage`]: the archives persisting finalized blocks and certificates.
//! - [`stack`]: assembles one validator's full stack (see its module docs for the
//!   component diagram).

pub mod application;
pub mod committer;
pub mod conformance;
pub mod execution;
pub mod idle_policy;
pub mod registry;
pub mod schedule;
pub mod stack;
pub mod storage;
pub mod types;

pub use execution::{BuildContext, ExecutionEnv};
pub use registry::{
    DerivationAttempt, DerivationLedger, DerivationSource, LOOKAHEAD_EPOCHS, RecordedDerivation,
    RecordedOutcome, RegistryObservation, RegistryReading, first_live_target, lookahead_height,
    replay_ledger, run_registry_derivation,
};
pub use schedule::{
    Committee, CommitteeSchedule, CommitteeSource, Governance, ScheduleEntry,
    ScheduledSchemeProvider,
};
pub use stack::{
    Channels, NullReporter, StackConfig, StackStart, ValidatorStack, engine_partition,
    start_validator,
};
