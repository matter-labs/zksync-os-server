//! The idle policy against real stack dynamics: an idle chain stops producing
//! blocks (leaders decline, views nullify and rotate), heartbeats bound the
//! silence, work wakes the chain promptly, and a pending committee change
//! sprints an idle chain to its boundary. The policy's decision table is
//! unit-tested next to the policy; these scenarios pin what consensus *does*
//! around those decisions.

use commonware_runtime::{Clock as _, Supervisor as _};
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use zksync_os_consensus_core::idle_policy::IdlePolicy;
use zksync_os_consensus_sim::{
    Behavior, EraOptions, IdleWork, MockExecution, SimCluster, links, run_scenario,
};

const EPOCH_LENGTH: u64 = 8;

fn short_epochs() -> zksync_os_consensus_sim::StackTuner {
    std::sync::Arc::new(|config| {
        config.epoch_length = NonZeroU64::new(EPOCH_LENGTH).expect("nonzero");
        // Idle scenarios are mostly nullified views; stretching the view
        // timeouts keeps the (virtual-time) churn from dominating wall time
        // without changing any behavior under test.
        config.leader_timeout = Duration::from_secs(5);
        config.certification_timeout = Duration::from_secs(5);
        config.timeout_retry = Duration::from_secs(10);
    })
}

/// The cluster's shared work pool, timed by the deterministic clock.
fn idle_work(clock: commonware_runtime::deterministic::Context, policy: IdlePolicy) -> IdleWork {
    IdleWork::new(
        policy,
        Arc::new(move || {
            clock
                .current()
                .duration_since(UNIX_EPOCH)
                .expect("deterministic clock starts at the epoch")
                .as_secs()
        }),
    )
}

/// An env attached to the shared pool.
fn idle_env(work: &IdleWork) -> MockExecution {
    let env = MockExecution::new();
    env.attach_idle(work.clone());
    env
}

/// Work makes blocks; no work makes none until the heartbeat interval passes,
/// then exactly one — the chain's cadence is demand plus a bounded pulse.
#[test]
fn an_idle_chain_declines_then_heartbeats() {
    run_scenario(
        "idle_declines_then_heartbeats",
        0..3,
        Duration::from_secs(3_600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; 4];
            let policy = IdlePolicy::heartbeat(
                Duration::from_secs(120),
                NonZeroU64::new(EPOCH_LENGTH).expect("nonzero"),
                Vec::new(),
            );
            let work = idle_work(context.child("idle_work"), policy);
            let cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| idle_env(&work),
                EraOptions {
                    stack_tuner: short_epochs(),
                    ..EraOptions::default()
                },
            )
            .await;

            // Three units of work: three blocks, then silence.
            work.enqueue(3);
            cluster.wait_for_committed_height_all(3).await;

            // Well inside the heartbeat interval: no new blocks.
            cluster.settle(Duration::from_secs(80)).await;
            for validator in 0..4 {
                assert_eq!(
                    cluster.validators[validator].env.committed_tip(),
                    Some(3),
                    "validator {validator} saw a block on an idle chain before the heartbeat",
                );
            }

            // Past the interval: the heartbeat block, and only it.
            cluster.wait_for_committed_height_all(4).await;
            cluster.settle(Duration::from_secs(80)).await;
            for validator in 0..4 {
                assert_eq!(
                    cluster.validators[validator].env.committed_tip(),
                    Some(4),
                    "validator {validator}: more than one heartbeat inside one interval",
                );
            }
            cluster.assert_committed_chains_agree(4);
        },
    );
}

/// A unit of work reaching the mempools wakes the chain long before the next
/// heartbeat would.
#[test]
fn work_wakes_an_idle_chain_promptly() {
    run_scenario(
        "idle_work_wakes",
        0..3,
        Duration::from_secs(3_600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; 4];
            let policy = IdlePolicy::heartbeat(
                // Long enough that any heartbeat inside this scenario would be
                // a bug in the wake path.
                Duration::from_secs(10_000),
                NonZeroU64::new(EPOCH_LENGTH).expect("nonzero"),
                Vec::new(),
            );
            let work = idle_work(context.child("idle_work"), policy);
            let cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| idle_env(&work),
                EraOptions {
                    stack_tuner: short_epochs(),
                    ..EraOptions::default()
                },
            )
            .await;

            work.enqueue(1);
            cluster.wait_for_committed_height_all(1).await;
            cluster.settle(Duration::from_secs(60)).await;

            // Idle, then one more unit of work: one more block, promptly.
            for validator in 0..4 {
                assert_eq!(cluster.validators[validator].env.committed_tip(), Some(1));
            }
            work.enqueue(1);
            cluster.wait_for_committed_height_all(2).await;
            cluster.settle(Duration::from_secs(60)).await;
            for validator in 0..4 {
                assert_eq!(cluster.validators[validator].env.committed_tip(), Some(2));
            }
            cluster.assert_committed_chains_agree(2);
        },
    );
}

/// A deployed-but-not-yet-active committee entry makes idle leaders produce at
/// full cadence until its epoch boundary passes — a scheduled change activates
/// without traffic — after which the chain is idle again, under the new
/// committee, and work proves the handoff live.
#[test]
fn a_pending_activation_sprints_an_idle_chain_to_its_boundary() {
    run_scenario(
        "idle_sprint_to_activation",
        0..3,
        Duration::from_secs(3_600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; 4];
            // Committee shrinks at epoch 2; the sprint target mirrors the
            // schedule, exactly as the node wires it from config.
            let schedule = vec![(0, vec![0, 1, 2, 3]), (2, vec![0, 1, 2])];
            let policy = IdlePolicy::heartbeat(
                // Heartbeats effectively off: what produces blocks below is
                // the sprint alone.
                Duration::from_secs(100_000),
                NonZeroU64::new(EPOCH_LENGTH).expect("nonzero"),
                vec![2],
            );
            let work = idle_work(context.child("idle_work"), policy);
            let cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| idle_env(&work),
                EraOptions {
                    stack_tuner: short_epochs(),
                    schedule,
                    ..EraOptions::default()
                },
            )
            .await;

            // No work enqueued at all: the sprint carries the chain through the
            // boundary block of epoch 1 (height 2*EPOCH_LENGTH - 1)...
            let boundary = 2 * EPOCH_LENGTH - 1;
            cluster.wait_for_committed_height_all(boundary).await;

            // ...and stops: the entry is active, normal idle rules resume.
            cluster.settle(Duration::from_secs(120)).await;
            for validator in 0..4 {
                assert_eq!(
                    cluster.validators[validator].env.committed_tip(),
                    Some(boundary),
                    "validator {validator} kept sprinting past the activation boundary",
                );
            }

            // Work proves the chain is live under the new (smaller) committee.
            work.enqueue(1);
            cluster.wait_for_committed_height_all(boundary + 1).await;
            cluster.assert_committed_chains_agree(boundary + 1);
        },
    );
}

/// Heartbeats alone eventually carry an idle chain across epoch boundaries:
/// rotation works at a crawl, one block per interval.
#[test]
fn heartbeats_cross_epoch_boundaries_at_a_crawl() {
    run_scenario(
        "idle_heartbeats_cross_boundaries",
        0..3,
        Duration::from_secs(7_200),
        |context| async move {
            let behaviors = vec![Behavior::Honest; 4];
            let policy = IdlePolicy::heartbeat(
                Duration::from_secs(45),
                NonZeroU64::new(EPOCH_LENGTH).expect("nonzero"),
                Vec::new(),
            );
            let work = idle_work(context.child("idle_work"), policy);
            let cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| idle_env(&work),
                EraOptions {
                    stack_tuner: short_epochs(),
                    ..EraOptions::default()
                },
            )
            .await;

            // A full epoch of nothing but heartbeats: the committee hands
            // engines over at a boundary reached one pulse at a time.
            cluster
                .wait_for_committed_height_all(EPOCH_LENGTH + 1)
                .await;
            cluster.assert_committed_chains_agree(EPOCH_LENGTH);
        },
    );
}
