//! Migration-at-height: consensus takes over a chain that already has history.
//!
//! The cutover model: the single sequencer is drained at an agreed height H, and the
//! committee starts with a consensus genesis *anchored* at H — a synthetic block
//! standing for the pre-consensus tip, derived identically by every validator from
//! the agreed anchor. The first block consensus ever decides is H+1. Nothing about
//! the pre-consensus history is re-sequenced or re-agreed: it is durable input, the
//! same way a chain's real genesis state is.
//!
//! These scenarios drive the *production* consensus stack over anchored genesis
//! blocks; what they prove is that nothing in the stack — engine, marshal delivery,
//! backfill, journals — silently assumes the chain root sits at height zero.

use commonware_runtime::Metrics as _;
use std::time::Duration;
use zksync_os_consensus_sim::{
    Behavior, EraOptions, MockExecution, SimCluster, links, run_scenario,
};

const NUM_VALIDATORS: usize = 5;
/// The agreed cutover height: the chain's pre-consensus era is this many blocks.
const ANCHOR: u64 = 20;

#[test]
fn committee_takes_over_a_preexisting_chain() {
    run_scenario(
        "migration_cutover",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            // Every validator's execution environment already holds the pre-consensus
            // chain (summarized by its anchor height, content-free in the mock), and
            // consensus is anchored at its tip.
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_with_env(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::anchored(ANCHOR),
            )
            .await;

            // The chain continues where the single sequencer stopped: the first
            // consensus-era blocks land at ANCHOR+1 onward, and every validator
            // commits the identical continuation.
            cluster.wait_for_committed_height_all(ANCHOR + 15).await;
            cluster.assert_committed_chains_agree(15);

            // A crash and restart on the migrated chain: the consensus journal and
            // archives were all born in the anchored era, and recovery must land the
            // validator back on the same continuation.
            cluster.crash(0);
            let survivors: Vec<usize> = (1..NUM_VALIDATORS).collect();
            cluster
                .wait_for_committed_height(&survivors, ANCHOR + 25)
                .await;
            cluster.restart(0).await;
            cluster.wait_for_committed_height_all(ANCHOR + 30).await;
            cluster.assert_committed_chains_agree(30);

            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

#[test]
fn chain_migrates_again_after_a_rollback() {
    run_scenario(
        "migration_re_cutover",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            // First era: consensus anchored at 20, runs for a while, then the whole
            // committee is stopped (the operational shape of a rollback: validators
            // halt, the single sequencer resumes from the same durable chain).
            let second_era_context = context.with_label("second_era");
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_with_env(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::anchored(ANCHOR),
            )
            .await;
            cluster.wait_for_committed_height_all(ANCHOR + 10).await;
            for index in 0..NUM_VALIDATORS {
                cluster.crash(index);
            }

            // The single-sequencer era in between produces more blocks; the second
            // migration anchors at the NEW tip. A fresh cluster with fresh consensus
            // state models the documented re-migration procedure (stale consensus
            // state from the first era must be cleared — the node refuses to mix
            // eras; that guard is pinned node-side).
            let second_anchor = ANCHOR + 10 + 7;
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_era(
                second_era_context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::anchored(second_anchor),
                EraOptions {
                    storage_prefix: "second-era-validator".to_string(),
                    ..EraOptions::default()
                },
            )
            .await;
            cluster
                .wait_for_committed_height_all(second_anchor + 10)
                .await;
            cluster.assert_committed_chains_agree(10);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

/// Real execution across the boundary: the pre-consensus era's state must be the
/// base the consensus era builds on — provably, not just structurally.
#[test]
fn migrated_chain_carries_its_state_across_the_boundary() {
    use alloy::primitives::U256;
    use zksync_os_consensus_sim::stf::{RealStfExecution, TEST_RECIPIENT, test_sender_address};

    run_scenario(
        "migration_real_stf",
        0..2,
        Duration::from_secs(600),
        |context| async move {
            // Eight blocks of real pre-consensus history (the single-sequencer era,
            // executed by the production VM outside consensus), then the committee
            // takes over at the anchor.
            const PRE: u64 = 8;
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_with_env(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| RealStfExecution::anchored(PRE),
            )
            .await;

            cluster.wait_for_committed_height_all(PRE + 6).await;

            // Transfer amounts encode absolute heights, so the recipient's balance
            // is determined by the chain height across BOTH eras: a committee that
            // lost the pre-history, or re-executed it differently, could not
            // produce this state.
            for &index in &cluster.honest_indices() {
                let env = &cluster.validators[index].env;
                let nonce = env
                    .committed_nonce(test_sender_address())
                    .expect("sender exists");
                assert!(nonce >= PRE + 6, "validator {index} is behind: {nonce}");
                assert_eq!(
                    env.committed_balance(TEST_RECIPIENT),
                    U256::from(nonce * (nonce + 1) / 2),
                    "validator {index}'s state does not include the pre-consensus era",
                );
            }
            cluster.assert_committed_chains_agree(6);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}
