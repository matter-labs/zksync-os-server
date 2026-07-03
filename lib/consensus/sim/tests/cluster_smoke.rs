//! Baseline scenarios for the full consensus stack: engine + marshal + gossip +
//! backfill + archives, driving a mock execution environment.
//!
//! The assertion that matters everywhere: **all honest validators commit the identical
//! chain** (agreement), it keeps growing (liveness), no honest validator produces fault
//! evidence, and nobody gets banned by the network layer. Every scenario runs over
//! several seeds and is executed twice per seed with bit-exact-reproduction asserted
//! (see `run_scenario`).

use std::time::Duration;
use zksync_os_consensus_sim::scenario::fingerprint;
use zksync_os_consensus_sim::{SimCluster, links, run_scenario};

const NUM_VALIDATORS: u32 = 5;

#[test]
fn five_validators_commit_identical_chains() {
    run_scenario(
        "steady_state",
        0..3,
        Duration::from_secs(300),
        |context| async move {
            let mut cluster = SimCluster::start(context, NUM_VALIDATORS, links::healthy()).await;
            cluster.wait_for_committed_height_all(25).await;
            cluster.assert_committed_chains_agree(25);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

#[test]
fn cluster_survives_degraded_links() {
    run_scenario(
        "degraded_links",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let mut cluster = SimCluster::start(context, NUM_VALIDATORS, links::degraded()).await;
            cluster.wait_for_committed_height_all(15).await;
            cluster.assert_committed_chains_agree(15);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

#[test]
fn crashed_validator_rejoins_and_catches_up() {
    run_scenario(
        "crash_restart",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let mut cluster = SimCluster::start(context, NUM_VALIDATORS, links::healthy()).await;

            // Everyone commits some history together.
            cluster.wait_for_committed_height_all(10).await;

            // One validator dies abruptly. Four of five remain — enough for quorum —
            // so the network keeps finalizing without it.
            cluster.crash(0);
            let survivors: Vec<usize> = (1..NUM_VALIDATORS as usize).collect();
            cluster.wait_for_committed_height(&survivors, 30).await;

            // The validator comes back over its surviving storage: its vote journal
            // replays (it cannot double-sign even though it crashed mid-view) and
            // marshal backfills the blocks it missed. It must converge on the exact
            // same chain.
            cluster.restart(0).await;
            cluster.wait_for_committed_height_all(40).await;
            cluster.assert_committed_chains_agree(40);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

/// Sanity check on the determinism machinery itself: fingerprints must actually capture
/// the execution, so different seeds (different interleavings) must produce different
/// fingerprints. Guards against the fingerprint degenerating into a constant.
#[test]
fn different_seeds_produce_different_executions() {
    let body = |context| async move {
        let cluster = SimCluster::start(context, NUM_VALIDATORS, links::healthy()).await;
        cluster.wait_for_committed_height_all(5).await;
    };
    let first = fingerprint(1, Duration::from_secs(300), &body);
    let second = fingerprint(2, Duration::from_secs(300), &body);
    assert_ne!(first, second, "different seeds should diverge");
}
