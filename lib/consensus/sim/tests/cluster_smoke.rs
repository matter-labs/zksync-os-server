//! First multi-validator scenarios for the full consensus stack: engine + marshal +
//! gossip + backfill + archives, driving a mock execution environment.
//!
//! The assertion that matters everywhere: **all validators commit the identical chain**
//! (agreement), it keeps growing (liveness), no honest validator produces fault evidence,
//! and nobody gets banned by the network layer. Plus the meta-assertion that makes this
//! suite trustworthy: identical seeds reproduce identical executions, bit for bit.

use commonware_p2p::simulated::Link;
use commonware_runtime::{Runner, deterministic};
use std::time::Duration;
use zksync_os_consensus_sim::SimCluster;

const NUM_VALIDATORS: u32 = 5;

fn healthy_link() -> Link {
    Link {
        latency: Duration::from_millis(20),
        jitter: Duration::from_millis(5),
        success_rate: 1.0,
    }
}

fn degraded_link() -> Link {
    Link {
        latency: Duration::from_millis(80),
        jitter: Duration::from_millis(40),
        success_rate: 0.9,
    }
}

#[test]
fn five_validators_commit_identical_chains() {
    let runner = deterministic::Runner::new(
        deterministic::Config::new()
            .with_seed(1)
            .with_timeout(Some(Duration::from_secs(300))),
    );
    runner.start(|context| async move {
        let mut cluster = SimCluster::start(context, NUM_VALIDATORS, healthy_link()).await;
        cluster.wait_for_committed_height_all(25).await;
        cluster.assert_committed_chains_agree(25);
        cluster.assert_no_faults();
        cluster.assert_no_blocked_peers().await;
    });
}

#[test]
fn cluster_survives_degraded_links() {
    let runner = deterministic::Runner::new(
        deterministic::Config::new()
            .with_seed(2)
            .with_timeout(Some(Duration::from_secs(600))),
    );
    runner.start(|context| async move {
        let mut cluster = SimCluster::start(context, NUM_VALIDATORS, degraded_link()).await;
        cluster.wait_for_committed_height_all(15).await;
        cluster.assert_committed_chains_agree(15);
        cluster.assert_no_faults();
        cluster.assert_no_blocked_peers().await;
    });
}

#[test]
fn crashed_validator_rejoins_and_catches_up() {
    let runner = deterministic::Runner::new(
        deterministic::Config::new()
            .with_seed(3)
            .with_timeout(Some(Duration::from_secs(600))),
    );
    runner.start(|context| async move {
        let mut cluster = SimCluster::start(context, NUM_VALIDATORS, healthy_link()).await;

        // Everyone commits some history together.
        cluster.wait_for_committed_height_all(10).await;

        // One validator dies abruptly. Four of five remain — above the two-thirds
        // quorum — so the network keeps finalizing without it.
        cluster.crash(0);
        let survivors: Vec<usize> = (1..NUM_VALIDATORS as usize).collect();
        cluster.wait_for_committed_height(&survivors, 30).await;

        // The validator comes back over its surviving storage: its vote journal replays
        // (it can not double-sign even though it crashed mid-view) and marshal backfills
        // the blocks it missed. It must converge on the exact same chain.
        cluster.restart(0).await;
        cluster.wait_for_committed_height_all(40).await;
        cluster.assert_committed_chains_agree(40);
        cluster.assert_no_faults();
        cluster.assert_no_blocked_peers().await;
    });
}

/// Runs a short cluster scenario and returns the runtime's execution fingerprint.
fn fingerprint_of_run(seed: u64) -> String {
    let runner = deterministic::Runner::new(
        deterministic::Config::new()
            .with_seed(seed)
            .with_timeout(Some(Duration::from_secs(300))),
    );
    runner.start(|context| async move {
        let auditor = context.auditor().clone();
        let cluster = SimCluster::start(context, NUM_VALIDATORS, healthy_link()).await;
        cluster.wait_for_committed_height_all(10).await;
        cluster.assert_committed_chains_agree(10);
        auditor.state()
    })
}

#[test]
fn same_seed_reproduces_the_exact_same_execution() {
    let first = fingerprint_of_run(7);
    let second = fingerprint_of_run(7);
    assert_eq!(first, second, "same seed must reproduce the same execution");

    let other = fingerprint_of_run(8);
    assert_ne!(first, other, "different seeds should diverge");
}
