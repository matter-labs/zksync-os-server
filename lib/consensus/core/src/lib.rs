//! BFT consensus core for zksync-os-server, built on [commonware](https://commonware.xyz)
//! simplex.
//!
//! This crate will contain the consensus subsystem: the application actor implementing
//! commonware's `Automaton`/`Relay`/`Reporter` traits, per-epoch engine management, marshal
//! wiring for ordered finalized-block delivery, and validator networking.
//!
//! Design rule: this crate depends only on commonware and the execution-seam traits — never
//! on the node's sequencer/state/storage crates. That is what lets the entire consensus
//! stack run under commonware's deterministic runtime in tests.
//!
//! Plan and architecture: `consensus_planning/` in the workspace root (see
//! `01-target-architecture.md` and `02-testing-strategy.md`).
//!
//! Current contents: the spike S3 smoke test (`tests/simplex_smoke.rs`) proving the pinned
//! commonware version runs a multi-validator simplex cluster deterministically inside this
//! workspace. The real modules land with Phase 1.
