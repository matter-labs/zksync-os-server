//! The on-chain validator registry under deterministic simulation: every honest
//! validator runs the production derivation driver and the production registry
//! parser over a manufactured registry chain state, and the scenarios pin the
//! properties the design leans on —
//!
//! - derivation is a pure function of chain state: every validator records the
//!   identical trail, live or catching up late;
//! - shadow mode is inert: consensus follows config no matter what the registry
//!   says, and an undeployed registry is quiet, not an alarm;
//! - config-shadow mode rotates committees from the registry, across a real
//!   epoch handoff into a member the config mirror does not list yet — the
//!   committee mechanics work and the lag is flagged as drift (production
//!   additionally loses that member's connectivity until the mirror deploys,
//!   which is outside the sim's reach — its network pre-links everyone);
//! - a broken registry (unknown layout, invalid proof of possession) blocks
//!   *rotation*, never the *chain* — and heals at the next boundary;
//! - restarts replay the recorded trail instead of re-deriving it.

use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;
use zksync_os_consensus_core::registry::RecordedOutcome;
use zksync_os_consensus_sim::registry::{
    registry_builder, registry_state, registry_state_with_bad_pop,
};
use zksync_os_consensus_sim::{
    Behavior, EraOptions, MockExecution, RegistrySpec, SimCluster, fingerprint, links, run_scenario,
};

/// Short epochs so a modest chain height crosses several lookahead boundaries:
/// with length 8, epoch `T`'s committee derives at height `(T−1)·8 − 1`.
const EPOCH_LENGTH: u64 = 8;

fn short_epochs() -> zksync_os_consensus_sim::StackTuner {
    Arc::new(|config| {
        *config = config
            .clone()
            .with_epoch_length(NonZeroU64::new(EPOCH_LENGTH).expect("nonzero"));
    })
}

#[test]
fn shadow_derivations_track_governance_and_agree_across_the_committee() {
    run_scenario(
        "registry_shadow",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; 4];
            let cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    registry: Some(RegistrySpec {
                        // Shadow: derivations record and compare, never govern.
                        flip_epoch: None,
                        timeline: Arc::new(|keys| {
                            // The registry does not exist until "governance
                            // deploys" it at height 10, with a schedule entry
                            // matching the config committee from epoch 4 on.
                            [(10, registry_state(keys, &[(4, vec![0, 1, 2, 3])]))]
                                .into_iter()
                                .collect()
                        }),
                    }),
                    ..EraOptions::default()
                },
            )
            .await;

            // Boundaries: epochs 0/1 derive at height 0, epoch T at (T−1)·8 − 1.
            // Height 36 passes epoch 5's boundary (31).
            cluster.wait_for_committed_height_all(36).await;
            cluster.wait_for_derivations(5).await;
            cluster.assert_committed_chains_agree(36);
            cluster.assert_no_faults();

            for validator in &cluster.validators {
                let registry = validator.registry.as_ref().expect("registry runs");
                let records = registry.ledger.records();
                // Epochs 0..=2 read pre-deployment state (all-zero: quiet, not
                // an alarm), epoch 3's boundary at height 15 sees the deployed
                // registry but no entry active that early; 4+ derive for real.
                assert!(records.len() >= 6, "expected epochs 0..=5 recorded");
                for record in &records[..4] {
                    assert_eq!(
                        record.outcome,
                        RecordedOutcome::CarriedNoEntry,
                        "pre-deployment epochs carry the config committee: {record:?}"
                    );
                }
                assert_eq!(records[4].outcome, RecordedOutcome::Derived);
                assert_eq!(records[5].outcome, RecordedOutcome::Derived);
                // Every derivation matched the config schedule — no drift.
                for observation in registry.observations.lock().unwrap().iter() {
                    assert!(
                        observation.matches_config,
                        "shadow must match config here: {observation:?}"
                    );
                }
            }
            // The trail is a pure function of chain state: byte-identical
            // records on every validator.
            let reference = cluster.validators[0]
                .registry
                .as_ref()
                .expect("registry runs")
                .ledger
                .records();
            for validator in &cluster.validators[1..] {
                assert_eq!(
                    validator
                        .registry
                        .as_ref()
                        .expect("registry runs")
                        .ledger
                        .records(),
                    reference,
                    "derivation trails diverged between validators"
                );
            }
        },
    );
}

#[test]
fn config_shadow_mode_grows_the_committee_from_the_registry() {
    run_scenario(
        "registry_config_shadow_growth",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; 5];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    // The config schedule never lists validator 4 — the mirror
                    // lagging behind a registry rotation. This scenario pins the
                    // committee mechanics of that state: the settled gate, engines
                    // over the derived committee, the boundary handoff, and the
                    // drift alarm. What it cannot pin is connectivity: the sim
                    // network pre-links every participant, while production builds
                    // its address book from config — there a lagging mirror also
                    // leaves validator 4 undialable until the mirror deploys (the
                    // node warns; see the peer-tracking TODO in node consensus.rs).
                    schedule: vec![(0, vec![0, 1, 2, 3])],
                    registry: Some(RegistrySpec {
                        flip_epoch: Some(2),
                        timeline: Arc::new(|keys| {
                            [(
                                0,
                                registry_state(
                                    keys,
                                    &[(2, vec![0, 1, 2, 3]), (4, vec![0, 1, 2, 3, 4])],
                                ),
                            )]
                            .into_iter()
                            .collect()
                        }),
                    }),
                    ..EraOptions::default()
                },
            )
            .await;

            // Epoch 4 starts at height 32; run well into it under the grown
            // committee.
            cluster.wait_for_committed_height_all(38).await;
            cluster.assert_committed_chains_agree(38);

            // Vote-again proof by quorum arithmetic: with five members and one
            // crashed, finalizing needs 4 of 4 remaining votes — including
            // validator 4's, which only the registry ever admitted.
            cluster.crash(0);
            let survivors: Vec<usize> = (1..5).collect();
            cluster.wait_for_committed_height(&survivors, 44).await;
            cluster.assert_no_faults();

            // The lagging mirror is not silent: every derivation whose committee
            // the config schedule does not list reports drift (epoch 4 grows to
            // five members, config still says four).
            for &index in &survivors {
                let registry = cluster.validators[index]
                    .registry
                    .as_ref()
                    .expect("registry runs");
                let observations = registry.observations.lock().unwrap();
                let grown = observations
                    .iter()
                    .find(|observation| observation.epoch == 4)
                    .expect("epoch 4 derived");
                assert!(
                    !grown.matches_config,
                    "a mirror missing the rotation must read as drift: {grown:?}"
                );
                assert_eq!(grown.committee.len(), 5);
            }
        },
    );
}

#[test]
fn a_broken_registry_blocks_rotation_never_the_chain() {
    run_scenario(
        "registry_refusal",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; 4];
            let cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    schedule: vec![(0, vec![0, 1, 2, 3])],
                    registry: Some(RegistrySpec {
                        flip_epoch: Some(2),
                        timeline: Arc::new(|keys| {
                            [
                                // Healthy at first: epoch 2 derives (boundary 7).
                                (0, registry_state(keys, &[(2, vec![0, 1, 2, 3])])),
                                // Governance "upgrades" the layout to a version
                                // this build does not parse before epoch 3's
                                // boundary (15): refuse to rotate.
                                (
                                    13,
                                    registry_builder(keys, &[(2, vec![0, 1, 2, 3])])
                                        .with_layout_version(2)
                                        .build(),
                                ),
                                // Rolled back, but with an identity whose proof
                                // of possession is signed for the wrong owner,
                                // scheduled from epoch 4 (boundary 23): refuse.
                                (
                                    20,
                                    registry_state_with_bad_pop(
                                        keys,
                                        &[(2, vec![0, 1, 2, 3]), (4, vec![0, 1, 2])],
                                        2,
                                    ),
                                ),
                                // Fixed for good before epoch 5's boundary (31).
                                (
                                    27,
                                    registry_state(
                                        keys,
                                        &[(2, vec![0, 1, 2, 3]), (5, vec![0, 1, 2, 3])],
                                    ),
                                ),
                            ]
                            .into_iter()
                            .collect()
                        }),
                    }),
                    ..EraOptions::default()
                },
            )
            .await;

            // The chain rides through both refusals and the recovery — five
            // epochs of it.
            cluster.wait_for_committed_height_all(44).await;
            cluster.wait_for_derivations(5).await;
            cluster.assert_committed_chains_agree(44);
            cluster.assert_no_faults();

            let registry = cluster.validators[0].registry.as_ref().expect("runs");
            let records = registry.ledger.records();
            let outcome_of = |epoch: u64| {
                records
                    .iter()
                    .find(|record| record.epoch == epoch)
                    .unwrap_or_else(|| panic!("no record for epoch {epoch}"))
            };
            assert_eq!(outcome_of(2).outcome, RecordedOutcome::Derived);
            assert_eq!(outcome_of(3).outcome, RecordedOutcome::CarriedRefused);
            assert_eq!(outcome_of(4).outcome, RecordedOutcome::CarriedRefused);
            assert_eq!(outcome_of(5).outcome, RecordedOutcome::Derived);
            // Refusals carried the last good committee forward, unchanged.
            assert_eq!(outcome_of(3).committee, outcome_of(2).committee);
            assert_eq!(outcome_of(4).committee, outcome_of(2).committee);
            // The refusal reasons surfaced (unknown layout, then the PoP).
            let observations = registry.observations.lock().unwrap();
            let refusal_of = |epoch: u64| {
                observations
                    .iter()
                    .find(|observation| observation.epoch == epoch)
                    .and_then(|observation| observation.refusal.clone())
                    .unwrap_or_else(|| panic!("no refusal for epoch {epoch}"))
            };
            assert!(
                refusal_of(3).contains("layout version"),
                "{}",
                refusal_of(3)
            );
            assert!(
                refusal_of(4).contains("proof of possession"),
                "{}",
                refusal_of(4)
            );
        },
    );
}

#[test]
fn a_lagging_validator_derives_the_identical_history_late() {
    run_scenario(
        "registry_laggard",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; 4];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    schedule: vec![(0, vec![0, 1, 2, 3])],
                    registry: Some(RegistrySpec {
                        flip_epoch: Some(2),
                        timeline: Arc::new(|keys| {
                            [(0, registry_state(keys, &[(2, vec![0, 1, 2, 3])]))]
                                .into_iter()
                                .collect()
                        }),
                    }),
                    ..EraOptions::default()
                },
            )
            .await;

            // Validator 3 sleeps through two lookahead boundaries (15 and 23)
            // and wakes inside a later epoch: its derivations run from
            // *historical* state during catch-up and must produce the trail the
            // others recorded live.
            cluster.wait_for_committed_height_all(6).await;
            cluster.crash(3);
            let survivors = [0, 1, 2];
            cluster.wait_for_committed_height(&survivors, 26).await;
            cluster.restart(3).await;
            cluster.wait_for_committed_height_all(34).await;
            cluster.wait_for_derivations(5).await;
            cluster.assert_committed_chains_agree(34);
            cluster.assert_no_faults();

            let reference = cluster.validators[0]
                .registry
                .as_ref()
                .expect("runs")
                .ledger
                .records();
            let laggard = cluster.validators[3]
                .registry
                .as_ref()
                .expect("runs")
                .ledger
                .records();
            assert!(reference.len() >= 4, "epochs 2..=5 expected: {reference:?}");
            assert_eq!(
                laggard, reference,
                "a late derivation must equal the live one"
            );
        },
    );
}

/// Semantic pin, run once per seed (`fingerprint` directly) instead of through
/// `run_scenario`'s bit-exactness double-run: the crash → provider-rebuild →
/// restart → second-crash choreography sits on the catch-up determinism gap
/// (see `promotion_catch_up_determinism_gap` in promotion.rs) — same-seed runs
/// converge to identical chains and identical derivation trails but can differ
/// in auditor fingerprint. Every registry property is still asserted inside the
/// body, every seed.
#[test]
fn a_restart_replays_the_recorded_trail_instead_of_rederiving() {
    for seed in 0..3 {
        let _ = fingerprint(seed, Duration::from_secs(600), &|context| async move {
            let behaviors = vec![Behavior::Honest; 4];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    schedule: vec![(0, vec![0, 1, 2, 3])],
                    registry: Some(RegistrySpec {
                        flip_epoch: Some(2),
                        timeline: Arc::new(|keys| {
                            [(0, registry_state(keys, &[(2, vec![0, 1, 2, 3])]))]
                                .into_iter()
                                .collect()
                        }),
                    }),
                    ..EraOptions::default()
                },
            )
            .await;

            // Record through epoch 4 (boundary 23), then restart validator 2
            // with a rebuilt provider — the process-restart shape: in-memory
            // committee knowledge is gone, only the ledger survives.
            cluster.wait_for_committed_height_all(26).await;
            cluster.crash(2);
            cluster.reconfigure_schedule(2, &vec![(0, vec![0, 1, 2, 3])]);
            cluster.restart(2).await;
            cluster.wait_for_committed_height_all(34).await;
            cluster.assert_committed_chains_agree(34);

            // The restarted validator still *votes* in registry-governed epochs
            // (quorum arithmetic: 3 of 4 with another member down needs it),
            // which is only possible if the replayed trail settled them.
            cluster.crash(0);
            let survivors = [1, 2, 3];
            cluster.wait_for_committed_height(&survivors, 40).await;
            cluster.assert_no_faults();

            // And it never re-derived a recorded epoch: one derive call per
            // epoch across both incarnations, none below the resume point after
            // the restart.
            let registry = cluster.validators[2].registry.as_ref().expect("runs");
            let calls = registry.derive_calls.lock().unwrap();
            let mut seen = std::collections::BTreeSet::new();
            for (epoch, _) in calls.iter() {
                assert!(
                    seen.insert(*epoch),
                    "epoch {epoch} was derived twice — the ledger replay is broken"
                );
            }
        });
    }
}
