//! Node-condition liveness: consensus running over validators whose *node* is
//! unhealthy — slow execution, lagging persistence — while the consensus protocol
//! itself is fine. The property under test is the separation: a degraded node must
//! degrade gracefully (late votes, lagging commits) without stalling the network,
//! and must converge once its condition clears or the others carry the chain.

use std::time::Duration;
use zksync_os_consensus_sim::{Behavior, DelayedEnv, MockExecution, SimCluster, run_scenario};

const NUM_VALIDATORS: usize = 5;

/// One validator's `verify` takes far longer than the leader timeout: its votes are
/// perpetually late. The other four are exactly quorum and keep finalizing; the
/// straggler still *commits* everything (finalized blocks arrive regardless of its
/// verification speed) and stays on the identical chain.
#[test]
fn slow_verifier_does_not_stall_the_network() {
    run_scenario(
        "slow_verifier",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = [Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_with_env(
                context,
                &behaviors,
                zksync_os_consensus_sim::links::healthy(),
                |index, context| {
                    let env = MockExecution::new();
                    if index == 4 {
                        // Well past the leader timeout: this validator's votes never
                        // arrive in time to matter.
                        DelayedEnv::slow_verify(env, context, Duration::from_secs(5))
                    } else {
                        DelayedEnv::slow_verify(env, context, Duration::ZERO)
                    }
                },
            )
            .await;

            // The four healthy validators carry the chain.
            cluster.wait_for_committed_height(&[0, 1, 2, 3], 20).await;
            // The straggler commits the same chain — late, but identically.
            cluster.wait_for_committed_height(&[4], 10).await;
            cluster.assert_committed_chains_agree(10);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

/// One validator's `commit` is slow — the sim analogue of a lagging persistence
/// pipeline. Commit speed must not affect voting: with three validators the quorum is
/// all of them, so the chain advancing at all *proves* the slow committer keeps
/// verifying and voting while its commits lag. Its chain converges once they drain
/// (consensus paces delivery to it via acknowledgements; it never skips).
#[test]
fn slow_committer_keeps_voting_and_converges() {
    run_scenario(
        "slow_committer",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = [Behavior::Honest; 3];
            let mut cluster = SimCluster::start_with_env(
                context,
                &behaviors,
                zksync_os_consensus_sim::links::healthy(),
                |index, context| {
                    let env = MockExecution::new();
                    if index == 0 {
                        // Each commit takes multiple block times (a block lands
                        // every ~100ms on healthy links).
                        DelayedEnv::slow_commit(env, context, Duration::from_millis(500))
                    } else {
                        DelayedEnv::slow_commit(env, context, Duration::ZERO)
                    }
                },
            )
            .await;

            // Quorum is 3-of-3: the fast validators reaching the target requires the
            // slow committer's votes on every single block.
            cluster.wait_for_committed_height(&[1, 2], 20).await;
            // The slow committer trails but commits the identical chain.
            cluster.wait_for_committed_height(&[0], 10).await;
            cluster.assert_committed_chains_agree(10);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}
