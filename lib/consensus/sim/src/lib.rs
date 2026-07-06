//! Deterministic simulation of the consensus stack.
//!
//! This crate exists so that consensus behavior — including crashes, restarts, bad
//! networks, and (eventually) Byzantine validators — can be tested exhaustively without
//! real processes, real time, or flakiness. It runs the *production* consensus stack
//! from `zksync_os_consensus_core`, swapping only the two boundaries that touch the
//! outside world:
//!
//! - the network → commonware's simulated p2p (link latency/jitter/loss under test
//!   control),
//! - the execution environment → [`MockExecution`], an in-memory chain.
//!
//! Everything runs inside commonware's deterministic runtime: single-threaded, seeded
//! scheduling, virtual time (timeouts cost nothing), in-memory storage that survives
//! simulated restarts, and an auditor hash that fingerprints the entire execution —
//! two runs with the same seed must produce identical fingerprints, which the test
//! suite asserts.

pub mod activity;
pub mod block;
pub mod cluster;
pub mod execution;
pub mod links;
pub mod scenario;
pub mod stf;

pub use activity::ActivityLog;
pub use block::SimBlock;
pub use cluster::{Behavior, SimCluster, SimValidator, StackTuner};
pub use execution::{DelayedEnv, MockExecution, SimEnv};
pub use scenario::run_scenario;
