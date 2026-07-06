//! Epoch rotation: consensus time is divided into fixed-length epochs, and one
//! simplex engine exists per epoch. Crossing a boundary means the stack starts the
//! next epoch's engine (its own vote journal, its own muxed slice of the consensus
//! channels) and retires the previous one once its tail is finalized — while
//! marshal, broadcast, and the execution environment run continuously underneath.
//!
//! The handoff itself is protocol-level: the new epoch's first proposal re-proposes
//! the previous epoch's boundary block, so the new engine begins by re-certifying
//! exactly where the old one stopped. These scenarios keep the committee identical
//! across epochs — they pin the *rotation machinery*; committee changes build on it.

use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;
use zksync_os_consensus_sim::{Behavior, MockExecution, SimCluster, links, run_scenario};

const NUM_VALIDATORS: usize = 5;
/// Short epochs so a modest chain height crosses several boundaries.
const EPOCH_LENGTH: u64 = 8;

fn short_epochs() -> zksync_os_consensus_sim::StackTuner {
    Arc::new(|config| {
        *config = config
            .clone()
            .with_epoch_length(NonZeroU64::new(EPOCH_LENGTH).expect("nonzero"));
    })
}

#[test]
fn engines_rotate_across_epoch_boundaries() {
    run_scenario(
        "epoch_rotation",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                &[],
                "validator",
                short_epochs(),
            )
            .await;

            // Height 30 with epochs of 8 crosses the boundaries at 8, 16, and 24 —
            // three full engine handoffs, each including the boundary re-proposal.
            cluster.wait_for_committed_height_all(30).await;
            cluster.assert_committed_chains_agree(30);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

#[test]
fn validator_restarts_across_an_epoch_boundary() {
    run_scenario(
        "epoch_rotation_restart",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                &[],
                "validator",
                short_epochs(),
            )
            .await;

            // Crash a validator inside epoch 0 and keep it down across the first
            // boundary: when it comes back the committee is deciding blocks in a
            // later epoch. Recovery must both replay its own epoch-0 journal (no
            // double-signing) and derive from its committed height which epoch's
            // engine it now needs — an engine it has never run before.
            cluster.wait_for_committed_height_all(4).await;
            cluster.crash(0);
            let survivors: Vec<usize> = (1..NUM_VALIDATORS).collect();
            cluster
                .wait_for_committed_height(&survivors, EPOCH_LENGTH + 4)
                .await;
            cluster.restart(0).await;
            cluster
                .wait_for_committed_height_all(2 * EPOCH_LENGTH + 4)
                .await;
            cluster.assert_committed_chains_agree(2 * EPOCH_LENGTH + 4);

            // Its votes count again: with a different validator stopped, the
            // remaining four (the restarted one included) are exactly quorum.
            cluster.crash(1);
            let with_restarted: Vec<usize> = vec![0, 2, 3, 4];
            cluster
                .wait_for_committed_height(&with_restarted, 3 * EPOCH_LENGTH)
                .await;
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}
