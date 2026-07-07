//! The EN-promotion storage shape: consensus starting over a *retained* chain.
//!
//! A node promoted from EN to validator arrives with the chain (execution state at
//! height H > 0) but without consensus history — no vote journals, no marshal
//! archives. This differs from every other recovery scenario in the corpus:
//! late join (`sync_join.rs`) has neither chain nor consensus state, and
//! crash-restart has both. Here the two stores disagree about where "the beginning"
//! is, and consensus must converge from its peers without touching the chain the
//! node already trusts.
//!
//! History: on commonware 2026.4.0 this shape was **not startable** — engines came
//! up assuming archived epoch-anchor blocks and marshal's `Inline` panicked on the
//! missing starting-epoch block (registered EN-promotion gap, probe of 2026-07-05).
//! On 2026.5.0 the rotation resolves each epoch's anchor from marshal *before*
//! spawning the engine, and these tests pin the result: the shape converges, and
//! the rebuilt validator votes again.

use std::num::NonZeroU64;
use std::time::Duration;
use zksync_os_consensus_sim::{
    Behavior, EraOptions, MockExecution, SimCluster, SimEnv as _, fingerprint, links, run_scenario,
};

const NUM_VALIDATORS: usize = 5;

/// Blocks per epoch in the boundary-crossing scenarios — short so the wiped window
/// spans several committees' worth of engine rotations.
const EPOCH_LENGTH: u64 = 8;

fn short_epochs() -> zksync_os_consensus_sim::StackTuner {
    std::sync::Arc::new(|config| {
        config.epoch_length = NonZeroU64::new(EPOCH_LENGTH).expect("nonzero");
    })
}

/// The base shape: wipe one validator's consensus state mid-run, keep its chain,
/// restart. It must converge back into the committee and vote again.
#[test]
fn fresh_consensus_over_retained_chain_converges() {
    run_scenario(
        "promotion_fresh_consensus_retained_chain",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions::default(),
            )
            .await;

            cluster.wait_for_committed_height_all(20).await;

            // The promotion storage shape: consensus state gone, chain retained.
            // (clear_consensus_state moves journals+archives to a fresh prefix;
            // not calling reset_env is the whole point of this scenario.)
            cluster.crash(4);
            cluster.clear_consensus_state(4);
            cluster.restart(4).await;

            // Convergence: the rebuilt member follows the chain again...
            cluster.wait_for_committed_height_all(35).await;
            cluster.assert_committed_chains_agree(30);

            // ...and participates. Stop a different validator: the remaining four
            // include the rebuilt one, and at n=5 quorum is four, so any further
            // finality requires its votes.
            cluster.crash(0);
            let with_rebuilt: Vec<usize> = vec![1, 2, 3, 4];
            cluster.wait_for_committed_height(&with_rebuilt, 45).await;

            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

/// The choreography of the realistic promotion shape, shared by the semantic pin
/// and the determinism reproducer below: the wipe-and-return spans epoch
/// boundaries, so the returning validator's fresh consensus state must cover
/// engine rotations it never ran — its current epoch's anchor block is deep in
/// history it never archived.
async fn wipe_and_return_across_boundaries(context: commonware_runtime::deterministic::Context) {
    let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
    let mut cluster = SimCluster::start_era(
        context,
        &behaviors,
        links::healthy(),
        |_index, _context| MockExecution::new(),
        EraOptions {
            stack_tuner: short_epochs(),
            ..EraOptions::default()
        },
    )
    .await;

    // Cross one boundary with everyone present, then take validator 4 out and
    // wipe it while the chain crosses two more without it.
    cluster
        .wait_for_committed_height_all(EPOCH_LENGTH + EPOCH_LENGTH / 2)
        .await;
    cluster.crash(4);
    cluster.clear_consensus_state(4);

    let running: Vec<usize> = (0..4).collect();
    cluster
        .wait_for_committed_height(&running, 3 * EPOCH_LENGTH + 2)
        .await;

    // Return with fresh consensus state over the retained chain, several epochs
    // behind. Tip discovery (scout), backfill across retired epochs, and rotation
    // must land it in the live epoch's engine.
    cluster.restart(4).await;
    cluster
        .wait_for_committed_height_all(4 * EPOCH_LENGTH + 2)
        .await;
    cluster.assert_committed_chains_agree(3 * EPOCH_LENGTH);

    // Vote-proof: with one other member stopped, the remaining four include the
    // rebuilt validator, and at n=5 quorum is four — further finality requires
    // its votes. The settle lets its residual archive backfill drain first.
    cluster.settle(Duration::from_secs(30)).await;
    cluster.crash(0);
    let with_rebuilt: Vec<usize> = vec![1, 2, 3, 4];
    cluster
        .wait_for_committed_height(&with_rebuilt, 5 * EPOCH_LENGTH)
        .await;

    cluster.assert_no_faults();
    cluster.assert_no_blocked_peers().await;
}

/// Semantic pin for the epoch-crossing promotion shape. Runs each seed once
/// (`fingerprint` directly) instead of through `run_scenario`'s bit-exactness
/// double-run: this choreography trips the registered teardown determinism gap —
/// same-seed runs converge to identical chains and identical logs but differ in
/// auditor fingerprint (see `promotion_catch_up_determinism_gap` below, and
/// `boundary_crash_determinism_gap` in reconfig.rs for the original register).
/// Every consensus property is still asserted inside the body, every seed.
#[test]
fn fresh_consensus_over_retained_chain_across_epoch_boundaries() {
    for seed in 0..3 {
        let _ = fingerprint(
            seed,
            Duration::from_secs(600),
            &wipe_and_return_across_boundaries,
        );
    }
}

/// The registered determinism gap, sharpened: the wiped-restart catch-up diverges
/// the auditor fingerprint without a crash landing mid-backfill — the catch-up
/// window itself (retired-epoch certificates re-heard through the tip scout,
/// racing marshal's processed-height floor drops) is enough. Kept ignored as the
/// on-demand reproducer for the upstream issue
/// (consensus_planning/upstream-issues.md #2); if this ever passes, the gap is
/// fixed and the semantic test above can graduate back to `run_scenario`.
#[test]
#[ignore = "registered determinism gap: wiped-restart catch-up across epochs; semantics unaffected"]
fn promotion_catch_up_determinism_gap() {
    run_scenario(
        "promotion_catch_up_determinism_gap",
        0..3,
        Duration::from_secs(600),
        wipe_and_return_across_boundaries,
    );
}

/// Floor-started rebuild, the base shape: wipe a validator's consensus state,
/// hand its restart a floor finalization from its own retained chain, and it
/// converges without refetching history below the floor — then votes again.
///
/// The floor here plays the role the node's finality store plays in production
/// (the sim harvests it from a healthy peer's activity log; the caller contract
/// is the same — at or below the retained tip).
#[test]
fn floor_started_rebuild_converges_and_votes() {
    run_scenario(
        "promotion_floor_started_rebuild",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions::default(),
            )
            .await;

            cluster.wait_for_committed_height_all(20).await;
            cluster.crash(4);
            cluster.clear_consensus_state(4);
            let floor = cluster.floor_at_or_below(0, 4);
            let floor_height = cluster.validators[4]
                .env
                .committed_chain_digests()
                .iter()
                .position(|digest| *digest == floor.proposal.payload)
                .expect("floor block is on the retained chain")
                as u64
                + 1;
            cluster.set_floor(4, floor);
            cluster.restart(4).await;

            cluster.wait_for_committed_height_all(35).await;
            cluster.assert_committed_chains_agree(30);

            // The bounded-catch-up property: history below the floor was never
            // fetched. (A genesis-started rebuild would hold every height.)
            assert!(
                floor_height > 2,
                "floor too low to make the below-floor probe meaningful"
            );
            assert!(
                !cluster.marshal_has_height(4, 1).await,
                "floor-started marshal fetched history below its floor"
            );
            assert!(
                cluster.marshal_has_height(4, floor_height + 1).await,
                "marshal is missing post-floor history it must have delivered"
            );

            // And it participates: quorum arithmetic as in the genesis-start test.
            cluster.crash(0);
            let with_rebuilt: Vec<usize> = vec![1, 2, 3, 4];
            cluster.wait_for_committed_height(&with_rebuilt, 45).await;

            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

/// Floor-started rebuild across epoch boundaries — the full promotion shape.
/// The floor lands mid-epoch in an epoch whose anchor block the validator can
/// never fetch (below the floor), so its first engine for that epoch starts
/// from the floor finalization itself (`Floor::Finalized` — the rotation's
/// floor path), and marshal catches up across the boundaries above it.
///
/// Single-run per seed (like `fresh_consensus_over_retained_chain_across_epoch_
/// boundaries` above, and for the same reason): the cross-epoch catch-up window
/// trips the registered fingerprint determinism gap on some seeds — less often
/// than the genesis-start variant (the floor bounds the window), but the
/// double-run gate cannot be relied on. Semantics are asserted every seed.
#[test]
fn floor_started_rebuild_across_epoch_boundaries() {
    for seed in 0..3 {
        let _ = fingerprint(seed, Duration::from_secs(600), &|context| async move {
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    ..EraOptions::default()
                },
            )
            .await;

            // Take validator 4 out mid-epoch-1 and wipe it; the chain crosses two
            // more boundaries without it.
            cluster
                .wait_for_committed_height_all(EPOCH_LENGTH + EPOCH_LENGTH / 2)
                .await;
            cluster.crash(4);
            cluster.clear_consensus_state(4);
            let running: Vec<usize> = (0..4).collect();
            cluster
                .wait_for_committed_height(&running, 3 * EPOCH_LENGTH + 2)
                .await;

            // The floor: the newest finalization on the wiped validator's retained
            // chain — mid-epoch-1, several epochs behind the live tip.
            let floor = cluster.floor_at_or_below(0, 4);
            cluster.set_floor(4, floor);
            cluster.restart(4).await;

            cluster
                .wait_for_committed_height_all(4 * EPOCH_LENGTH + 2)
                .await;
            cluster.assert_committed_chains_agree(3 * EPOCH_LENGTH);
            assert!(
                !cluster.marshal_has_height(4, 1).await,
                "floor-started marshal fetched history below its floor"
            );

            // Vote-proof.
            cluster.settle(Duration::from_secs(30)).await;
            cluster.crash(0);
            let with_rebuilt: Vec<usize> = vec![1, 2, 3, 4];
            cluster
                .wait_for_committed_height(&with_rebuilt, 5 * EPOCH_LENGTH)
                .await;

            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        });
    }
}
