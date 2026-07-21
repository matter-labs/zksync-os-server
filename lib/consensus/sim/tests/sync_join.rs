//! Late join: a validator that is part of the committee from genesis but starts its
//! process only after the chain has real history. It begins with nothing — no vote
//! journal, no block archives, no state — and must backfill everything from its peers
//! before it can participate.
//!
//! This differs from the crash/restart scenarios (`cluster_smoke.rs`, `real_stf.rs`):
//! a restarted validator recovers *its own* storage and fills a bounded gap, while a
//! late joiner replays the entire chain from other validators' archives. It is the
//! simulation shape of the v1 operational plan for new validators — provision the
//! committee membership first, deploy the node later.

use alloy::primitives::U256;
use std::time::Duration;
use zksync_os_consensus_sim::stf::{RealStfExecution, TEST_RECIPIENT, test_sender_address};
use zksync_os_consensus_sim::{Behavior, MockExecution, SimCluster, links, run_scenario};

const NUM_VALIDATORS: usize = 5;

#[test]
fn late_starting_validator_backfills_everything_and_votes() {
    run_scenario(
        "late_join_backfill_and_vote",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            // Validator 4 is provisioned (committee member, keys) but not started.
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_with_env_stopped(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                &[4],
            )
            .await;

            // The four running validators are exactly quorum at n=5: the chain must
            // grow a real history without the absent member.
            let running: Vec<usize> = (0..4).collect();
            cluster.wait_for_committed_height(&running, 20).await;

            // First start, from nothing. Marshal backfills the whole chain from the
            // peers' archives and delivers it in order; the joiner's committed chain
            // must converge on the same blocks everyone else has.
            cluster.restart(4).await;
            cluster.wait_for_committed_height_all(30).await;
            cluster.assert_committed_chains_agree(30);

            // Catching up is not participating — prove the joiner *votes*. Stop a
            // different validator: the remaining four include the joiner, and at n=5
            // quorum is four, so any further finality requires the joiner's votes.
            cluster.crash(0);
            let with_joiner: Vec<usize> = vec![1, 2, 3, 4];
            cluster.wait_for_committed_height(&with_joiner, 40).await;

            cluster.assert_committed_chains_agree(30);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

#[test]
fn late_starting_validator_reexecutes_history_into_identical_state() {
    run_scenario(
        "late_join_real_stf",
        0..2,
        Duration::from_secs(600),
        |context| async move {
            // Same shape over real execution: the joiner has no state layers at all,
            // so every backfilled block goes through the commit path's re-execution
            // against its own (initially genesis) state.
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_with_env_stopped(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| RealStfExecution::new(),
                &[4],
            )
            .await;

            let running: Vec<usize> = (0..4).collect();
            cluster.wait_for_committed_height(&running, 6).await;

            cluster.restart(4).await;
            cluster.wait_for_committed_height_all(10).await;

            // The joiner rebuilt the chain's state purely by re-executing history:
            // balances and nonces must match the canonical per-height transfer
            // schedule on every validator, the joiner included.
            for &index in &cluster.honest_indices() {
                let env = &cluster.validators[index].env;
                let nonce = env
                    .committed_nonce(test_sender_address())
                    .expect("sender exists");
                assert!(nonce >= 10, "validator {index} is behind: nonce {nonce}");
                assert_eq!(
                    env.committed_balance(TEST_RECIPIENT),
                    U256::from(nonce * (nonce + 1) / 2),
                    "validator {index} rebuilt a different state than the chain agreed on",
                );
            }
            cluster.assert_committed_chains_agree(10);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}
