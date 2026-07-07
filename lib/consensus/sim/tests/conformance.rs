//! The simulation environments run the shared [`ExecutionEnv`] contract checks
//! against themselves. They are the reference implementations the checks were
//! distilled from, so these tests mostly pin the *reference* in place — the
//! production environment runs the identical checks in its own suite, and that
//! pairing is what closes the "production deviates from the model" bug class.

use std::time::Duration;
use zksync_os_consensus_core::conformance::commit_and_redelivery_contract;
use zksync_os_consensus_core::execution::{BuildContext, ExecutionEnv};
use zksync_os_consensus_sim::stf::RealStfExecution;
use zksync_os_consensus_sim::{MockExecution, run_scenario};

/// Builds a chain of `count` blocks through the environment's own `build`,
/// exactly as a sequence of leader turns would.
async fn build_chain<X: ExecutionEnv>(env: &mut X, count: u64) -> Vec<X::Block> {
    let mut blocks = Vec::new();
    let mut parent = env.genesis_block().await;
    for view in 1..=count {
        let block = env
            .build(parent.clone(), BuildContext { epoch: 0, view })
            .await
            .expect("reference environments build on demand");
        blocks.push(block.clone());
        parent = block;
    }
    blocks
}

#[test]
fn mock_env_honors_the_commit_contract() {
    run_scenario(
        "conformance_mock",
        0..1,
        Duration::from_secs(60),
        |_context| async move {
            let mut env = MockExecution::new();
            let blocks = build_chain(&mut env, 5).await;
            commit_and_redelivery_contract(&mut env, blocks, 0).await;
        },
    );
}

#[test]
fn anchored_mock_env_honors_the_commit_contract() {
    run_scenario(
        "conformance_mock_anchored",
        0..1,
        Duration::from_secs(60),
        |_context| async move {
            // A migrated chain: 100 blocks of pre-consensus history.
            let mut env = MockExecution::anchored(100);
            let blocks = build_chain(&mut env, 5).await;
            commit_and_redelivery_contract(&mut env, blocks, 100).await;
        },
    );
}

#[test]
fn real_stf_env_honors_the_commit_contract() {
    run_scenario(
        "conformance_real_stf",
        0..1,
        Duration::from_secs(120),
        |_context| async move {
            let mut env = RealStfExecution::new();
            let blocks = build_chain(&mut env, 4).await;
            commit_and_redelivery_contract(&mut env, blocks, 0).await;
        },
    );
}

#[test]
fn anchored_real_stf_env_honors_the_commit_contract() {
    run_scenario(
        "conformance_real_stf_anchored",
        0..1,
        Duration::from_secs(120),
        |_context| async move {
            let mut env = RealStfExecution::anchored(7);
            let blocks = build_chain(&mut env, 4).await;
            commit_and_redelivery_contract(&mut env, blocks, 7).await;
        },
    );
}
