//! The executable half of the [`ExecutionEnv`](crate::ExecutionEnv) contract.
//!
//! The simulation environments are reference implementations of the contract;
//! the production environment must behave identically wherever consensus can
//! observe the difference — and precisely there no simulation can catch a
//! deviation, because the sims *are* the model. This module states the
//! contract's load-bearing clauses as checks that every implementation's own
//! test suite runs against itself: the mock, the real-STF sim env, and the
//! production node env alike. A new environment (or a change to an existing
//! one) that passes its unit tests but breaks these checks is exactly the bug
//! class this module exists to stop.
//!
//! Two clauses are covered, both learned the hard way (an operational review
//! found both violated in production while every sim honored them):
//!
//! - **Redelivery tolerance.** Commit delivery is at-least-once and may reach
//!   *below* the committed tip by more than one block: marshal resumes from
//!   its own durable marker (synced per ack batch, so it can trail the
//!   environment's durable tip after an unclean kill), and a finality-floor
//!   restart re-delivers from the floor block itself, which floor selection
//!   deliberately allows below the tip. Redelivered known blocks must be
//!   absorbed as no-ops — never a panic, never a state change.
//! - **Era-relative heights.** `committed_height` speaks consensus heights:
//!   the era anchor is height 0, the first consensus-decided block is
//!   height 1. Rotation, marshal archive lookups, and floor windows all
//!   consume it under that convention; a chain-absolute answer works on fresh
//!   chains (anchor 0) and dooms every migrated one.

use crate::execution::ExecutionEnv;
use commonware_consensus::Heightable as _;

/// Drives `env` through the commit-and-redelivery contract over `blocks` — a
/// valid, already-built chain whose first entry sits directly on the era
/// anchor. The caller builds the chain by whatever means its environment
/// needs (the trait's own `build`, a hand-rolled VM harness, fixtures);
/// everything after that point is contract, not construction.
///
/// Checks, in order:
/// 1. committing the chain reports era-relative heights after every commit;
/// 2. re-delivering the tip is a no-op;
/// 3. re-delivering the *entire chain from the first block* — plural
///    redelivery below the tip, the unclean-kill and floor-restart shape — is
///    absorbed without panic and without moving the committed height.
pub async fn commit_and_redelivery_contract<X: ExecutionEnv>(
    env: &mut X,
    blocks: Vec<X::Block>,
    era_anchor: u64,
) {
    assert!(!blocks.is_empty(), "the contract needs at least one block");
    let chain_len = blocks.len() as u64;
    // `Heightable::height` is era-relative by convention (the anchor is zero);
    // `era_anchor` documents the chain shape being driven and pins the caller
    // to a consistent setup.
    let _ = era_anchor;
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(
            block.height().get(),
            index as u64 + 1,
            "conformance driver misuse: blocks must sit directly on the anchor",
        );
    }

    for (index, block) in blocks.iter().enumerate() {
        env.commit(block.clone()).await;
        let committed = env.committed_height().await;
        assert_eq!(
            committed.map(|height| height.get()),
            Some(index as u64 + 1),
            "committed_height must be era-relative (anchor = 0) after commit {}",
            index + 1,
        );
    }

    // Tip redelivery: the classic at-least-once shape.
    env.commit(blocks.last().expect("nonempty").clone()).await;
    assert_eq!(
        env.committed_height().await.map(|height| height.get()),
        Some(chain_len),
        "tip redelivery must not move the committed height",
    );

    // Plural redelivery from below the tip: what marshal actually does after
    // an unclean kill (its marker trails) and on floor restarts (delivery
    // resumes at the floor block itself).
    for block in &blocks {
        env.commit(block.clone()).await;
    }
    assert_eq!(
        env.committed_height().await.map(|height| height.get()),
        Some(chain_len),
        "redelivery of already-final blocks must be absorbed as no-ops",
    );
}
