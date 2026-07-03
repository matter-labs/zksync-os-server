//! Real execution in simulation: blocks carry actual signed transactions, and every
//! validator runs them through the production VM (the exact code path the sequencer's
//! replay uses).
//!
//! What this buys over [`MockExecution`](crate::execution::MockExecution):
//!
//! - **Verify-before-vote is real**: a follower re-executes the leader's block against
//!   the parent state and compares the execution-outcome hash before voting. A leader
//!   whose block misdeclares its outcome gets rejected by execution, not by convention.
//! - **State equality is assertable**: after a run, tests read balances and nonces from
//!   each validator's committed state through the production state-view traits and
//!   assert they are identical everywhere.
//!
//! Everything stays in memory and inside the deterministic runtime: the genesis state is
//! derived once per process from the production genesis input, blocks execute in a few
//! milliseconds, and runs remain bit-exactly reproducible.

mod block;
mod execution;
mod genesis;

pub use block::StfBlock;
pub use execution::{RealStfExecution, TEST_RECIPIENT, test_sender_address};
