//! Replica-determinism (history-invariance) properties for registry
//! derivations: **any two replicas with different operational histories over
//! the same chain must agree on every governed committee.**
//!
//! Committee resolution is consensus-critical — if two honest validators
//! answer "who holds epoch E" differently, the committee splits. The chain
//! state and the config are consensus-uniform inputs; a validator's *ledger
//! trail* is not (it depends on when the node joined, which mode it ran
//! before, and which boundaries it slept through). These tests pin that the
//! derivation pipeline never lets that node-local history leak into a
//! governed answer.
//!
//! The harness runs the production derivation driver
//! ([`run_registry_derivation`]) and the production registry parser (via
//! [`SlotMapSource`]) — no consensus engines, no cluster — so a case costs
//! milliseconds and the property sweep gets real volume. Each replica is a
//! sequence of process incarnations ("phases") sharing a durable ledger,
//! with the mode wiring copied from the node
//! (node/bin/src/consensus/mod.rs `initial_target`): shadow mode skips
//! boundaries missed while down, config_shadow resumes densely from the
//! flip.

use commonware_codec::ReadExt as _;
use commonware_cryptography::Signer as _;
use commonware_cryptography::bls12381::primitives::group;
use commonware_cryptography::bls12381::primitives::ops::compute_public;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::ed25519;
use commonware_runtime::{Clock as _, Handle, Spawner as _, Supervisor as _};
use commonware_utils::TryFromIterator as _;
use proptest::prelude::*;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;
use zksync_os_consensus_core::registry::{
    DerivationLedger as _, RecordedDerivation, RecordedOutcome,
};
use zksync_os_consensus_core::schedule::{CommitteeSchedule, ScheduleEntry};
use zksync_os_consensus_core::{
    CommitteeSource, first_live_target, replay_ledger, run_registry_derivation,
};
use zksync_os_consensus_sim::fingerprint;
use zksync_os_consensus_sim::registry::{MemoryLedger, RegistryTimeline, SlotMapSource};

/// Cluster size. Config lists everyone; registry entries pick subsets.
const VALIDATORS: usize = 4;

/// Deterministic key material (no OS randomness — cases must replay).
fn keys() -> Vec<(ed25519::PrivateKey, group::Private)> {
    (0..VALIDATORS)
        .map(|index| {
            let network = ed25519::PrivateKey::read(&mut [index as u8 + 1; 32].as_slice())
                .expect("32 bytes are a seed");
            let bls = (0u8..=255)
                .find_map(|salt| {
                    let mut bytes = [index as u8 + 1; 32];
                    bytes[0] = salt;
                    group::Private::read(&mut bytes.as_slice()).ok()
                })
                .expect("some salt yields a canonical scalar");
            (network, bls)
        })
        .collect()
}

/// The config schedule: one epoch-0 entry with every validator — the totality
/// guarantee, and the mirror the carry falls back to before any recording.
fn config_schedule(keys: &[(ed25519::PrivateKey, group::Private)]) -> CommitteeSchedule {
    let committee = commonware_utils::ordered::BiMap::try_from_iter(
        keys.iter()
            .map(|(network, bls)| (network.public_key(), compute_public::<MinPk>(bls))),
    )
    .expect("distinct validators");
    CommitteeSchedule::new(vec![ScheduleEntry {
        activation_epoch: 0,
        committee,
    }])
    .expect("epoch-0 entry present")
}

/// One process incarnation's mode.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    /// `shadow`: derivations record but never govern; startup skips boundaries
    /// that passed while the node was down.
    Shadow,
    /// `config_shadow` with this flip epoch: recordings govern from it on;
    /// startup resumes densely from the flip.
    ConfigShadow { flip: u64 },
}

/// A replica's operational history: process incarnations over a shared durable
/// ledger. `run_until_epoch` = keep this incarnation until the trail covers
/// that epoch (the applied height feed advances far enough to allow it).
#[derive(Debug, Clone)]
struct Phase {
    mode: Mode,
    run_until_epoch: Option<u64>,
    applied_height: u64,
}

/// Runs one phase: fresh `CommitteeSource` (a restart forgets memory), ledger
/// replay, the node's `initial_target` wiring, then the production driver
/// until the trail covers `run_until_epoch` (or immediately, for phases that
/// exist only to model downtime).
async fn run_phase(
    context: commonware_runtime::deterministic::Context,
    label: String,
    epoch_length: NonZeroU64,
    schedule: CommitteeSchedule,
    timeline: Arc<RegistryTimeline>,
    ledger: MemoryLedger,
    phase: Phase,
) {
    let Some(until) = phase.run_until_epoch else {
        return;
    };
    let source = match phase.mode {
        Mode::Shadow => CommitteeSource::from_config(schedule),
        Mode::ConfigShadow { flip } => CommitteeSource::with_registry_from(schedule, flip),
    };
    let newest = replay_ledger(&source, &ledger.load().expect("memory ledger loads"));
    // The node's resume rules, verbatim (node/bin/src/consensus/mod.rs).
    let initial_target = match phase.mode {
        Mode::ConfigShadow { flip } => newest.map_or(flip, |newest| (newest + 1).max(flip)),
        Mode::Shadow => {
            let live = first_live_target(0, epoch_length, phase.applied_height);
            let resume = newest.map_or(live, |newest| newest + 1);
            resume.max(live)
        }
    };
    let applied = phase.applied_height;
    let driver: Handle<()> = context.child("driver").spawn({
        let ledger = ledger.clone();
        move |ctx| {
            run_registry_derivation(
                ctx,
                0,
                epoch_length,
                initial_target,
                move || Some(applied),
                SlotMapSource::new(timeline),
                ledger,
                source,
                |_observation| {},
            )
        }
    });
    // The driver derives every boundary at or below the applied height; wait
    // for the trail to cover this phase's horizon, then "crash" the process.
    loop {
        if ledger.records().iter().any(|record| record.epoch >= until) {
            break;
        }
        context.sleep(Duration::from_millis(100)).await;
    }
    driver.abort();
    tracing::debug!(phase = %label, "phase complete");
}

/// A replica's whole history against one chain, producing its final trail.
async fn run_replica(
    context: commonware_runtime::deterministic::Context,
    name: String,
    epoch_length: NonZeroU64,
    schedule: CommitteeSchedule,
    timeline: Arc<RegistryTimeline>,
    phases: Vec<Phase>,
) -> Vec<RecordedDerivation> {
    let ledger = MemoryLedger::default();
    for (index, phase) in phases.into_iter().enumerate() {
        run_phase(
            context.child("phase"),
            format!("{name}#{index}"),
            epoch_length,
            schedule.clone(),
            timeline.clone(),
            ledger.clone(),
            phase,
        )
        .await;
    }
    ledger.records()
}

/// The lookahead boundary for `epoch` (era anchor 0) — where its derivation
/// reads chain state.
fn boundary(epoch_length: NonZeroU64, epoch: u64) -> u64 {
    zksync_os_consensus_core::lookahead_height(0, epoch_length, epoch)
}

/// The oracle: every governed epoch each replica recorded must carry the
/// identical committee on all replicas that recorded it.
fn assert_governed_agreement(
    flip: u64,
    horizon: u64,
    trails: &[(String, Vec<RecordedDerivation>)],
) {
    for epoch in flip..=horizon {
        let answers: Vec<(&str, &RecordedDerivation)> = trails
            .iter()
            .filter_map(|(name, trail)| {
                trail
                    .iter()
                    .find(|record| record.epoch == epoch)
                    .map(|record| (name.as_str(), record))
            })
            .collect();
        assert!(
            !answers.is_empty(),
            "no replica recorded governed epoch {epoch}"
        );
        let (reference_name, reference) = answers[0];
        for (name, record) in &answers[1..] {
            assert!(
                record.committee == reference.committee,
                "committee split at governed epoch {epoch}: replica `{name}` \
                 ({:?}, {} members) disagrees with replica `{reference_name}` \
                 ({:?}, {} members) — a consensus-critical answer depended on \
                 node-local history",
                record.outcome,
                record.committee.len(),
                reference.outcome,
                reference.committee.len(),
            );
        }
    }
}

/// The concrete shape from the review finding, pinned deterministically:
/// a `shadow` → `config_shadow` rollout where the registry drifted from config
/// during the shadow era and then breaks (layout bump) before the flip
/// epoch's boundary. A veteran carries its newest shadow recording; a node
/// that joined at the flip has no shadow trail and must reach the same
/// answer anyway.
#[test]
fn a_flip_epoch_refusal_carries_the_same_committee_on_every_history() {
    let _ = fingerprint(0, Duration::from_secs(3_600), &|context| async move {
        let keys = keys();
        let epoch_length = NonZeroU64::new(4).expect("nonzero");
        let flip = 3;
        let horizon = 4;
        let schedule = config_schedule(&keys);
        // Registry: from genesis, a valid entry at epoch 1 with a *subset*
        // committee {0,1,2} (drifted from config's {0,1,2,3}); sabotaged with
        // an unknown layout version just before epoch 3's boundary — every
        // governed derivation refuses and must carry.
        let timeline: Arc<RegistryTimeline> = Arc::new(
            [
                (
                    0,
                    zksync_os_consensus_sim::registry::registry_state(&keys, &[(1, vec![0, 1, 2])]),
                ),
                (
                    boundary(epoch_length, flip) - 1,
                    zksync_os_consensus_sim::registry::registry_builder(
                        &keys,
                        &[(1, vec![0, 1, 2])],
                    )
                    .with_layout_version(2)
                    .build(),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let veteran = run_replica(
            context.child("veteran"),
            "shadow-veteran".into(),
            epoch_length,
            schedule.clone(),
            timeline.clone(),
            vec![
                // Ran shadow mode through the pre-flip era...
                Phase {
                    mode: Mode::Shadow,
                    run_until_epoch: Some(flip - 1),
                    applied_height: boundary(epoch_length, flip - 1),
                },
                // ...then the operator flipped the mode.
                Phase {
                    mode: Mode::ConfigShadow { flip },
                    run_until_epoch: Some(horizon),
                    applied_height: boundary(epoch_length, horizon),
                },
            ],
        )
        .await;
        let fresh = run_replica(
            context.child("fresh"),
            "fresh-joiner".into(),
            epoch_length,
            schedule.clone(),
            timeline.clone(),
            vec![Phase {
                mode: Mode::ConfigShadow { flip },
                run_until_epoch: Some(horizon),
                applied_height: boundary(epoch_length, horizon),
            }],
        )
        .await;

        // Sanity: the scenario is what it claims — the governed epochs refused.
        for trail in [&veteran, &fresh] {
            let at_flip = trail
                .iter()
                .find(|record| record.epoch == flip)
                .expect("flip epoch recorded");
            assert_eq!(at_flip.outcome, RecordedOutcome::CarriedRefused);
        }

        assert_governed_agreement(
            flip,
            horizon,
            &[
                ("shadow-veteran".into(), veteran),
                ("fresh-joiner".into(), fresh),
            ],
        );
    });
}

/// The property sweep over the class: registry timelines (drift, refusals,
/// missing entries — sabotage placed anywhere around the flip) × replica
/// lifecycles (fresh joiners, shadow veterans, veterans with downtime gaps).
/// Whatever the combination, governed committees must agree.
#[derive(Debug, Clone)]
struct EquivalencePlan {
    epoch_length: u64,
    flip: u64,
    /// A registry schedule entry present from genesis: (activation, members).
    /// `None` = the registry never gains an entry (NoEntry everywhere).
    entry: Option<(u64, Vec<usize>)>,
    /// Sabotage the registry state (unknown layout version) from this epoch's
    /// boundary on. Around the flip is where it bites.
    sabotage_from_epoch: Option<u64>,
    lifecycles: Vec<LifecyclePlan>,
}

#[derive(Debug, Clone, Copy)]
enum LifecyclePlan {
    Fresh,
    ShadowVeteran,
    /// A veteran that slept across boundaries mid-shadow: its first shadow
    /// incarnation stops after `stop_after` epochs and the next one resumes
    /// with the chain already past `resume_gap` further boundaries (shadow
    /// skips them — coverage, not custody).
    ShadowWithGap {
        stop_after: u64,
        resume_gap: u64,
    },
}

fn plan_strategy() -> impl Strategy<Value = EquivalencePlan> {
    (
        2u64..=4,
        2u64..=4,
        prop::option::of((
            0u64..=2,
            prop::sample::subsequence(vec![0usize, 1, 2, 3], 2..=4),
        )),
        prop::option::of(0u64..=5),
        prop::collection::vec(
            prop_oneof![
                Just(LifecyclePlan::Fresh),
                Just(LifecyclePlan::ShadowVeteran),
                (1u64..=2, 1u64..=2).prop_map(|(stop_after, resume_gap)| {
                    LifecyclePlan::ShadowWithGap {
                        stop_after,
                        resume_gap,
                    }
                }),
            ],
            2..=3,
        ),
    )
        .prop_map(
            |(epoch_length, flip, entry, sabotage_from_epoch, lifecycles)| EquivalencePlan {
                epoch_length,
                flip,
                entry,
                sabotage_from_epoch,
                lifecycles,
            },
        )
}

fn check_plan(plan: &EquivalencePlan) {
    let _ = fingerprint(0, Duration::from_secs(3_600), &|context| {
        let plan = plan.clone();
        async move {
            let keys = keys();
            let epoch_length = NonZeroU64::new(plan.epoch_length).expect("nonzero");
            let flip = plan.flip;
            let horizon = flip + 2;
            let schedule = config_schedule(&keys);

            let entries: Vec<(u64, Vec<usize>)> = plan.entry.clone().into_iter().collect();
            let mut timeline: RegistryTimeline = [(
                0,
                zksync_os_consensus_sim::registry::registry_state(&keys, &entries),
            )]
            .into_iter()
            .collect();
            if let Some(epoch) = plan.sabotage_from_epoch {
                timeline.insert(
                    boundary(epoch_length, epoch).saturating_sub(1),
                    zksync_os_consensus_sim::registry::registry_builder(&keys, &entries)
                        .with_layout_version(2)
                        .build(),
                );
            }
            let timeline = Arc::new(timeline);

            let mut trails = Vec::new();
            for (index, lifecycle) in plan.lifecycles.iter().enumerate() {
                let name = format!("replica-{index}-{lifecycle:?}");
                let phases = match *lifecycle {
                    LifecyclePlan::Fresh => vec![Phase {
                        mode: Mode::ConfigShadow { flip },
                        run_until_epoch: Some(horizon),
                        applied_height: boundary(epoch_length, horizon),
                    }],
                    LifecyclePlan::ShadowVeteran => vec![
                        Phase {
                            mode: Mode::Shadow,
                            run_until_epoch: Some(flip.saturating_sub(1)),
                            applied_height: boundary(epoch_length, flip.saturating_sub(1)),
                        },
                        Phase {
                            mode: Mode::ConfigShadow { flip },
                            run_until_epoch: Some(horizon),
                            applied_height: boundary(epoch_length, horizon),
                        },
                    ],
                    LifecyclePlan::ShadowWithGap {
                        stop_after,
                        resume_gap,
                    } => {
                        let stop_at = stop_after.min(flip.saturating_sub(1));
                        let resume_at = (stop_at + resume_gap).min(flip.saturating_sub(1));
                        vec![
                            Phase {
                                mode: Mode::Shadow,
                                run_until_epoch: Some(stop_at),
                                applied_height: boundary(epoch_length, stop_at),
                            },
                            Phase {
                                mode: Mode::Shadow,
                                run_until_epoch: Some(resume_at),
                                applied_height: boundary(epoch_length, resume_at),
                            },
                            Phase {
                                mode: Mode::ConfigShadow { flip },
                                run_until_epoch: Some(horizon),
                                applied_height: boundary(epoch_length, horizon),
                            },
                        ]
                    }
                };
                let trail = run_replica(
                    context.child("replica"),
                    name.clone(),
                    epoch_length,
                    schedule.clone(),
                    timeline.clone(),
                    phases,
                )
                .await;
                trails.push((name, trail));
            }

            assert_governed_agreement(flip, horizon, &trails);
        }
    });
}

fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        // Driver-only cases cost milliseconds; volume is affordable here.
        .unwrap_or(64);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn replicas_with_any_history_agree_on_governed_committees(plan in plan_strategy()) {
        check_plan(&plan);
    }
}
