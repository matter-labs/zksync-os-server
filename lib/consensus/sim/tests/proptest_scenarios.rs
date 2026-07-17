//! Property-based scenario sweep: random *combinations* of configuration
//! dimensions, checked against configuration-independent oracles.
//!
//! The handwritten scenarios each pin one behavior under one configuration.
//! What they structurally cannot catch is a bug that needs two unremarkable
//! dimensions to line up — the class this file exists for. (The concrete
//! ancestor: the idle policy computed epochs from absolute chain heights,
//! which is only wrong when `era anchor ≠ 0` *and* an activation is pending —
//! each dimension individually well-tested, the combination never exercised.)
//!
//! proptest generates a [`ScenarioPlan`] — pure data — and the plan is
//! executed once under the deterministic runtime, seeded from the plan
//! itself. Failures therefore reproduce exactly, and proptest's shrinking
//! walks the plan toward a minimal counterexample (fewer validators, anchor
//! toward 0, less work); a dimension survives shrinking only if the failure
//! genuinely needs it, so the reported plan *is* the diagnosis.
//!
//! The oracles hold for every generated plan:
//!
//! - **Activation-by-deadline**: a scheduled committee change on an idle
//!   chain activates without traffic (by sprint, heartbeat crawl, or legacy
//!   cadence — whichever the plan's policy provides).
//! - **Sprint self-limit**: with heartbeats off, a sprinting chain stops at
//!   exactly the activation boundary.
//! - **Heartbeat liveness**: a heartbeat-mode chain produces its first pulse
//!   without any help.
//! - **Work liveness**: enqueued work becomes blocks, under every policy and
//!   committee shape.
//! - **Agreement, no faults**: committed chains are prefix-identical across
//!   validators and no honest validator records fault evidence.
//!
//! Knobs: `PROPTEST_CASES` scales the number of plans (default is small — a
//! few plans per PR keep the cost of one test file; the nightly lane can
//! sweep hundreds). Failing plans persist to
//! `proptest_scenarios.proptest-regressions` next to this file — commit that
//! file when it grows, it replays the exact counterexamples first on every
//! future run.

use commonware_runtime::{Clock as _, Supervisor as _};
use proptest::prelude::*;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use zksync_os_consensus_core::idle_policy::IdlePolicy;
use zksync_os_consensus_sim::{
    Behavior, EraOptions, IdleWork, MockExecution, SimCluster, fingerprint, links,
};

/// A heartbeat interval far past the virtual horizon: heartbeats off, the
/// sprint (or explicit work) is the only thing that can produce a block.
const SPRINT_ONLY_INTERVAL: u64 = 1_000_000;

/// Virtual-time budget per plan. The worst legitimate plan is a heartbeat
/// crawl to an activation boundary (≤ 7 blocks, each one interval ≤ 16 s
/// plus a few 5 s views of leader routing — ≈ 300 s) plus settles and work
/// rounds. A plan that cannot finish in this budget has a liveness bug; the
/// runtime panics and proptest shrinks the plan. Deliberately snug: a
/// *failing* plan stalls in nullified-view churn for the whole budget, and
/// that churn — re-run per shrink attempt — is what a failure costs in wall
/// time.
const VIRTUAL_TIMEOUT: Duration = Duration::from_secs(480);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyPlan {
    /// Idle leaders always build (the pre-policy behavior).
    Legacy,
    /// Decline / heartbeat / sprint, with this interval. Activation epochs
    /// are always wired from the schedule, exactly as the node derives them
    /// from its committee config.
    Heartbeat { interval_secs: u64 },
}

/// One point in configuration space; everything the scenario body needs, and
/// nothing else — so the printed counterexample is a complete reproducer.
#[derive(Debug, Clone)]
struct ScenarioPlan {
    /// Seed for the deterministic runtime: scheduling, key minting, network.
    sim_seed: u64,
    validators: usize,
    /// Pre-consensus history length; 0 is a fresh chain. The historically
    /// under-tested dimension — generation is biased toward nonzero.
    era_anchor: u64,
    epoch_length: u64,
    policy: PolicyPlan,
    /// A committee change (drop the last validator) activating at this epoch.
    activation_epoch: Option<u64>,
    /// Work batches enqueued sequentially, each waited to become blocks.
    work_bursts: Vec<u64>,
}

impl ScenarioPlan {
    fn policy(&self) -> IdlePolicy {
        let epoch_length = NonZeroU64::new(self.epoch_length).expect("plan epochs are nonzero");
        match self.policy {
            PolicyPlan::Legacy => IdlePolicy::legacy(),
            PolicyPlan::Heartbeat { interval_secs } => IdlePolicy::heartbeat(
                Duration::from_secs(interval_secs),
                epoch_length,
                // All schedule entries, like the node: the epoch-0 entry is
                // inert (already active), a pending one arms the sprint.
                std::iter::once(0).chain(self.activation_epoch).collect(),
            ),
        }
    }

    /// The committee schedule: everyone from epoch 0, and — when the plan has
    /// an activation — everyone but the last validator from that epoch on.
    fn schedule(&self) -> Vec<(u64, Vec<usize>)> {
        let everyone: Vec<usize> = (0..self.validators).collect();
        let mut schedule = vec![(0, everyone.clone())];
        if let Some(epoch) = self.activation_epoch {
            schedule.push((epoch, everyone[..self.validators - 1].to_vec()));
        }
        schedule
    }

    /// Whether nothing but the sprint (or explicit work) can produce blocks.
    fn sprint_only(&self) -> bool {
        matches!(
            self.policy,
            PolicyPlan::Heartbeat {
                interval_secs: SPRINT_ONLY_INTERVAL
            }
        )
    }
}

fn plan_strategy() -> impl Strategy<Value = ScenarioPlan> {
    (
        any::<u64>(),
        3usize..=5usize,
        // Biased toward migrated chains; shrinks toward fresh (variant order),
        // so an anchor survives in a counterexample only if the bug needs it.
        prop_oneof![1 => Just(0u64), 3 => 1u64..=100_000u64],
        2u64..=4u64,
        prop_oneof![
            1 => Just(PolicyPlan::Legacy),
            2 => (8u64..=16u64).prop_map(|interval_secs| PolicyPlan::Heartbeat { interval_secs }),
            2 => Just(PolicyPlan::Heartbeat { interval_secs: SPRINT_ONLY_INTERVAL }),
        ],
        prop::option::of(1u64..=2u64),
        prop::collection::vec(1u64..=3u64, 0..=2),
    )
        .prop_map(
            |(
                sim_seed,
                validators,
                era_anchor,
                epoch_length,
                policy,
                activation_epoch,
                work_bursts,
            )| {
                ScenarioPlan {
                    sim_seed,
                    validators,
                    era_anchor,
                    epoch_length,
                    policy,
                    activation_epoch,
                    work_bursts,
                }
            },
        )
}

/// Executes one plan under the deterministic runtime and checks every oracle
/// the plan is entitled to. Panics (inside the runtime) fail the case.
///
/// Single execution per plan — the double-run determinism proof lives in
/// `run_scenario` and does not need repeating on every generated case.
fn check_plan(plan: &ScenarioPlan) {
    fingerprint(plan.sim_seed, VIRTUAL_TIMEOUT, &|context| {
        let plan = plan.clone();
        async move {
            let behaviors = vec![Behavior::Honest; plan.validators];
            let clock = context.child("idle_work");
            let work = IdleWork::new(
                plan.policy(),
                Arc::new(move || {
                    clock
                        .current()
                        .duration_since(UNIX_EPOCH)
                        .expect("deterministic clock starts at the epoch")
                        .as_secs()
                }),
            );
            let anchor = plan.era_anchor;
            let cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| {
                    let env = MockExecution::anchored(anchor);
                    env.attach_idle(work.clone());
                    env
                },
                EraOptions {
                    stack_tuner: {
                        let epoch_length = plan.epoch_length;
                        Arc::new(move |config| {
                            config.epoch_length =
                                NonZeroU64::new(epoch_length).expect("plan epochs are nonzero");
                            // Stretched view timeouts: idle stretches are
                            // nullified views, and churning them faster only
                            // burns wall-clock (see the idle scenarios).
                            config.leader_timeout = Duration::from_secs(5);
                            config.certification_timeout = Duration::from_secs(5);
                            config.timeout_retry = Duration::from_secs(10);
                        })
                    },
                    schedule: plan.schedule(),
                    ..EraOptions::default()
                },
            )
            .await;

            // Activation-by-deadline: with no work at all, the chain must
            // reach the boundary block of the pending entry — sprint at full
            // cadence, heartbeat one interval at a time, legacy by cadence.
            if let Some(epoch) = plan.activation_epoch {
                let boundary = anchor + epoch * plan.epoch_length - 1;
                cluster.wait_for_committed_height_all(boundary).await;

                // Sprint self-limit: with heartbeats off nothing else may
                // produce, so after the boundary the chain stands still.
                if plan.sprint_only() {
                    cluster.settle(Duration::from_secs(12)).await;
                    for index in cluster.honest_indices() {
                        assert_eq!(
                            cluster.committed_height(index),
                            boundary,
                            "validator {index} built past the activation boundary \
                             with heartbeats off",
                        );
                    }
                }
            } else if !plan.sprint_only() {
                // Heartbeat liveness (legacy cadence likewise): an idle chain
                // with a finite pulse produces its first block unaided.
                cluster.wait_for_committed_height_all(anchor + 1).await;
            }

            // Work liveness: every burst becomes blocks on every validator —
            // including any validator the activation dropped from the
            // committee, which must keep following the chain.
            for &burst in &plan.work_bursts {
                let base = cluster
                    .honest_indices()
                    .iter()
                    .map(|&index| cluster.committed_height(index))
                    .min()
                    .expect("cluster has validators")
                    .max(anchor);
                work.enqueue(burst);
                cluster.wait_for_committed_height_all(base + burst).await;
            }

            let era_blocks = cluster
                .honest_indices()
                .iter()
                .map(|&index| cluster.committed_height(index))
                .min()
                .expect("cluster has validators")
                .saturating_sub(anchor);
            cluster.assert_committed_chains_agree(era_blocks);
            cluster.assert_no_faults();
        }
    });
}

fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        // Each case boots a full cluster and runs blocks through real
        // consensus (seconds, not microseconds): a handful of plans per PR
        // run; sweeps belong to the nightly lane via PROPTEST_CASES.
        .unwrap_or(8);
    ProptestConfig {
        cases,
        // Every shrink attempt is a full scenario run, and attempts that
        // still fail cost a whole stalled-scenario budget each; cap the walk
        // so a failure reports in minutes. 16 attempts minimize well across
        // this plan's seven dimensions.
        max_shrink_iters: 16,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn any_configuration_upholds_liveness_activation_and_agreement(plan in plan_strategy()) {
        check_plan(&plan);
    }
}
