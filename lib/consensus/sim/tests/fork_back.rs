//! The disaster hardfork under deterministic simulation: an operator-coordinated
//! era change that abandons finalized blocks above an agreed height N.
//!
//! What these scenarios pin is the fork's *arithmetic* — the property that makes
//! an operator-coordinated fork safe without any in-protocol signaling:
//!
//! - a quorum that adopts the new era produces a live chain from N+1 on;
//! - stragglers restarted on the dead era disrupt only themselves — below
//!   quorum, that era can never finalize another block;
//! - a below-quorum fork produces *no* live chain on either side — the fork's
//!   constitutional cost, executable.
//!
//! What they deliberately do not pin: the protocol-version handshake partition
//! (the sim network has no handshake — the L3 drill covers "cannot even pair"),
//! and the truncation tool's storage surgery (unit-tested where it lives; the
//! sim's `fork_to` models its outcome — a chain re-anchored at N).

use alloy::primitives::U256;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;
use zksync_os_consensus_sim::stf::{RealStfExecution, TEST_RECIPIENT, test_sender_address};
use zksync_os_consensus_sim::{
    Behavior, EraOptions, MockExecution, SimCluster, SimEnv, fingerprint, links, run_scenario,
};

const NUM_VALIDATORS: usize = 5;
/// Short epochs so the doomed era rotates before the fork — the new era must
/// restart epoch numbering from its own anchor regardless of where the old one
/// stood.
const EPOCH_LENGTH: u64 = 8;

fn short_epochs() -> zksync_os_consensus_sim::StackTuner {
    Arc::new(|config| {
        *config = config
            .clone()
            .with_epoch_length(NonZeroU64::new(EPOCH_LENGTH).expect("nonzero"));
    })
}

#[test]
fn a_quorum_fork_abandons_the_suffix_and_lives() {
    run_scenario(
        "fork_back_quorum",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            use commonware_runtime::Supervisor as _;
            let fork_context = context.child("fork");

            // The doomed era runs past an epoch boundary; the operators then
            // declare everything above height 9 poisoned and halt the world.
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut doomed = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    storage_prefix: "era-one".to_string(),
                    ..EraOptions::default()
                },
            )
            .await;
            doomed.wait_for_committed_height_all(12).await;
            doomed.assert_committed_chains_agree(12);
            let envs: Vec<MockExecution> = doomed
                .validators
                .iter()
                .map(|validator| validator.env.clone())
                .collect();
            for index in 0..NUM_VALIDATORS {
                doomed.crash(index);
            }

            // Every operator runs the truncation and adopts the fork config:
            // the same chain re-anchored at 9, fresh consensus storage (the
            // runbook clears the engine state — a fresh prefix is the sim
            // spelling of that).
            for env in &envs {
                env.fork_to(9);
            }
            let mut fork = SimCluster::start_era(
                fork_context,
                &behaviors,
                links::healthy(),
                |index, _context| envs[index].clone(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    storage_prefix: "era-two".to_string(),
                    ..EraOptions::default()
                },
            )
            .await;

            // The new era finalizes from N+1 on, agrees, and crosses its own
            // epoch boundaries counted from the fork anchor. (Waits speak
            // chain-absolute heights; the agreement assert counts the era's
            // own blocks.)
            fork.wait_for_committed_height_all(9 + EPOCH_LENGTH + 4).await;
            fork.assert_committed_chains_agree(EPOCH_LENGTH + 4);
            fork.assert_no_faults();
            fork.assert_no_blocked_peers().await;
        },
    );
}

/// Semantic pin, run once per seed (`fingerprint` directly) instead of through
/// `run_scenario`'s bit-exactness double-run: the crash → dead-era restart →
/// second-crash → fork-adoption choreography sits on the registered catch-up
/// determinism gap (see `promotion_catch_up_determinism_gap` in promotion.rs
/// and upstream-issues.md #2) — same-seed runs converge to identical chains
/// but can differ in auditor fingerprint. Every fork property is still
/// asserted inside the body, every seed.
#[test]
fn adoption_below_quorum_freezes_both_eras_until_a_quorum_forms() {
    for seed in 0..3 {
        let _ = fingerprint(seed, Duration::from_secs(600), &|context| async move {
            use commonware_runtime::Supervisor as _;
            let fork_context = context.child("fork");

            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut doomed = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    storage_prefix: "era-one".to_string(),
                    ..EraOptions::default()
                },
            )
            .await;
            doomed.wait_for_committed_height_all(10).await;
            let envs: Vec<MockExecution> = doomed
                .validators
                .iter()
                .map(|validator| validator.env.clone())
                .collect();
            for index in 0..NUM_VALIDATORS {
                doomed.crash(index);
            }

            // Three operators fork (below the quorum of four); two miss the
            // memo and restart on the dead era — same identities, same
            // journals, which is exactly what a real straggler resumes.
            for env in envs.iter().take(3) {
                env.fork_to(7);
            }
            let mut fork = SimCluster::start_era(
                fork_context,
                &behaviors,
                links::healthy(),
                |index, _context| envs[index].clone(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    storage_prefix: "era-two".to_string(),
                    // Validators 3 and 4 never adopted: provisioned, not started.
                    stopped: vec![3, 4],
                    ..EraOptions::default()
                },
            )
            .await;
            doomed.restart(3).await;
            doomed.restart(4).await;

            // Neither side moves: the fork holds three of the four required
            // votes, the dead era holds two. This is D-HF1's teeth — with no
            // in-protocol override, a disputed fork halts rather than forks
            // the network in two.
            fork.assert_no_progress_for(&[0, 1, 2], Duration::from_secs(60))
                .await;
            doomed
                .assert_no_progress_for(&[3, 4], Duration::from_secs(60))
                .await;

            // A fourth operator comes around: stop its dead-era node, run the
            // truncation, adopt. Quorum forms and the new era lives — while
            // the last straggler stays frozen, disrupting nobody.
            doomed.crash(3);
            envs[3].fork_to(7);
            fork.restart(3).await;
            fork.wait_for_committed_height(&[0, 1, 2, 3], 7 + 5).await;
            // The last straggler's environment still holds the dead era's
            // chain — agreement is asserted between the adopters.
            fork.assert_committed_chains_agree_between(&[0, 1, 2, 3], 5);
            fork.assert_no_faults();
            doomed
                .assert_no_progress_for(&[4], Duration::from_secs(30))
                .await;
        });
    }
}

/// After committing height H, the recipient holds 1 + 2 + ... + H wei — each
/// block's transfer amount encodes its height (see the real-STF backend).
fn expected_recipient_balance(height: u64) -> U256 {
    U256::from(height * (height + 1) / 2)
}

/// Every validator's committed *state* agrees and matches its own chain height:
/// recipient balance and sender nonce read through the production state-view
/// traits. (The mock scenarios above prove the chain *sequence*; this proves the
/// state behind it.) `minimum_height` is chain-absolute — the balance spans both
/// eras because the pre-fork history carries over in the anchor's state.
fn assert_state_agreement(cluster: &SimCluster<RealStfExecution>, minimum_height: u64) {
    let honest = cluster.honest_indices();
    // Every validator in the cluster shares the era anchor (they all forked to
    // the same height); the committed chain restarts numbering above it.
    let anchor = cluster.validators[honest[0]].env.era_anchor();
    for &index in &honest {
        let env = &cluster.validators[index].env;
        // One transfer per block, so the sender's nonce is the committed
        // (chain-absolute) height — the pre-fork transfers live in the anchor.
        let height = env
            .committed_nonce(test_sender_address())
            .expect("sender exists");
        assert!(
            height >= minimum_height,
            "validator {index} committed height {height} below expected {minimum_height}"
        );
        assert_eq!(
            env.committed_balance(TEST_RECIPIENT),
            expected_recipient_balance(height),
            "validator {index} recipient balance does not match its chain height"
        );
    }
    // Chain agreement counts the era's own blocks (heights restart per era).
    cluster.assert_committed_chains_agree(minimum_height - anchor);
}

/// The same fork, over *real* execution: what the mock scenarios prove about the
/// block sequence, this proves about committed *state*. Every validator forks
/// back to the real state at N, re-executes the discarded window and beyond, and
/// the recipient's balance must re-converge to exactly the per-height
/// accumulation — proof that the discarded blocks' effects re-land
/// deterministically (the sim's analog of L1 deposit-cursor re-ingestion; the
/// L1 side is the node drill in `integration-tests/tests/node/fork.rs`).
///
/// All-crash-then-all-fork, like the quorum scenario above, so it is bit-exact
/// across `run_scenario`'s double run (no straggler on the dead era to hit the
/// catch-up determinism gap).
#[test]
fn a_real_stf_fork_reexecutes_to_identical_state() {
    run_scenario(
        "fork_back_real_stf",
        0..2,
        Duration::from_secs(600),
        |context| async move {
            use commonware_runtime::Supervisor as _;
            let fork_context = context.child("fork");
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];

            // The doomed era: real signed transfers, one per block, past an
            // epoch boundary. Everything above height 6 is about to be declared
            // poisoned.
            let mut doomed = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| RealStfExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    storage_prefix: "era-one".to_string(),
                    ..EraOptions::default()
                },
            )
            .await;
            doomed.wait_for_committed_height_all(10).await;
            assert_state_agreement(&doomed, 10);
            let envs: Vec<RealStfExecution> = doomed
                .validators
                .iter()
                .map(|validator| validator.env.clone())
                .collect();
            for index in 0..NUM_VALIDATORS {
                doomed.crash(index);
            }

            // Fork back to height 6, re-anchoring every validator's real state
            // there; fresh consensus storage is the sim spelling of the runbook's
            // "clear the engine state" step.
            for env in &envs {
                env.fork_to(6);
            }
            let mut fork = SimCluster::start_era(
                fork_context,
                &behaviors,
                links::healthy(),
                |index, _context| envs[index].clone(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    storage_prefix: "era-two".to_string(),
                    ..EraOptions::default()
                },
            )
            .await;

            // The new era re-executes past the old tip. `assert_state_agreement`
            // pins the payoff: every validator's committed balance equals the
            // per-height accumulation `1 + 2 + ... + H` — so blocks 7..=10 (the
            // discarded window) re-landed with identical effects, and the chain
            // continued from there, all off the real state at the fork anchor.
            fork.wait_for_committed_height_all(12).await;
            assert_state_agreement(&fork, 12);
            fork.assert_no_faults();
            fork.assert_no_blocked_peers().await;
        },
    );
}
