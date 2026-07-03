//! Network partition scenarios.
//!
//! The safety obligation under partition is absolute: a side without quorum must commit
//! *nothing*, no matter how long the partition lasts — a fork (two sides both extending
//! the chain) is the one unforgivable failure. Liveness is allowed to suffer while the
//! network is split and must return once it heals.
//!
//! With 5 validators, quorum is 4: tolerating 1 byzantine fault means agreement needs
//! 4 matching votes. So a 4-validator side keeps committing, while a 3-validator side
//! (or smaller) must stall.

use std::time::Duration;
use zksync_os_consensus_sim::{SimCluster, links, run_scenario};

const NUM_VALIDATORS: u32 = 5;

#[test]
fn isolated_validator_stalls_while_majority_continues() {
    run_scenario(
        "isolate_one",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let mut cluster = SimCluster::start(context, NUM_VALIDATORS, links::healthy()).await;
            cluster.wait_for_committed_height_all(10).await;

            // Cut one validator off. The remaining four are exactly quorum, so the
            // chain keeps growing without it.
            cluster.partition(&[&[0], &[1, 2, 3, 4]]).await;
            cluster.wait_for_committed_height(&[1, 2, 3, 4], 25).await;

            // Blocks finalized just before the cut may still be in flight to the
            // isolated validator; let those drain before demanding stillness.
            cluster.settle(Duration::from_secs(10)).await;

            // The isolated validator must stand still: it can neither produce blocks
            // (no quorum for its proposals) nor learn of new finalizations.
            cluster
                .assert_no_progress_for(&[0], Duration::from_secs(20))
                .await;

            // Heal. The isolated validator backfills what it missed and rejoins.
            cluster.heal(links::healthy()).await;
            cluster.wait_for_committed_height_all(40).await;
            cluster.assert_committed_chains_agree(40);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

#[test]
fn even_split_halts_everyone_then_heals() {
    run_scenario(
        "even_split",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let mut cluster = SimCluster::start(context, NUM_VALIDATORS, links::healthy()).await;
            cluster.wait_for_committed_height_all(10).await;

            // Split 2 / 3: neither side reaches the quorum of 4. Blocks finalized just
            // before the cut may still be delivered just after it, so let those drain
            // first. From then on: if any validator commits anything during the split,
            // quorum arithmetic is broken and forks are possible — the stillness IS
            // the safety property.
            cluster.partition(&[&[0, 1], &[2, 3, 4]]).await;
            let everyone: Vec<usize> = (0..NUM_VALIDATORS as usize).collect();
            cluster.settle(Duration::from_secs(10)).await;
            cluster
                .assert_no_progress_for(&everyone, Duration::from_secs(30))
                .await;

            // Heal: liveness must come back, and everyone converges on one chain.
            cluster.heal(links::healthy()).await;
            cluster.wait_for_committed_height_all(20).await;
            cluster.assert_committed_chains_agree(20);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}
