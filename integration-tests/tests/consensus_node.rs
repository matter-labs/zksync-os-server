use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Context as _;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::provider::ZksyncTestingProvider;

const CLUSTER_FORMATION_TIMEOUT: Duration = Duration::from_secs(20);
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(20);
const L1_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(60);

#[path = "consensus_node/restarted_node_catchup.rs"]
mod restarted_node_catchup;

fn consensus_test_keys(n: usize) -> Vec<zksync_os_network::SecretKey> {
    (0..n)
        .map(|_| zksync_os_network::rng_secret_key())
        .collect()
}

async fn raft_status(
    cluster: &MultiNodeTester,
    index: usize,
) -> anyhow::Result<zksync_os_status_server::StatusResponse> {
    cluster.node(index).status().await
}

fn raft_node_id(
    status: &zksync_os_status_server::StatusResponse,
    index: usize,
) -> anyhow::Result<String> {
    status
        .consensus
        .raft
        .as_ref()
        .map(|raft| raft.node_id.clone())
        .ok_or_else(|| anyhow::anyhow!("node {index} did not expose raft status"))
}

fn raft_last_applied(
    status: &zksync_os_status_server::StatusResponse,
    index: usize,
) -> anyhow::Result<Option<u64>> {
    status
        .consensus
        .raft
        .as_ref()
        .map(|raft| raft.last_applied_index)
        .ok_or_else(|| anyhow::anyhow!("node {index} did not expose raft status"))
}

async fn send_transfer(
    cluster: &MultiNodeTester,
    index: usize,
) -> anyhow::Result<alloy::rpc::types::TransactionReceipt> {
    let node = cluster.node(index);
    send_transfer_to_node(node).await
}

async fn send_transfer_to_node(
    node: &Tester,
) -> anyhow::Result<alloy::rpc::types::TransactionReceipt> {
    let gas_price = node.l2_provider.get_gas_price().await?;
    let tx = TransactionRequest::default()
        .with_to(Address::random())
        .with_value(U256::from(1))
        .with_gas_price(gas_price);
    node.l2_provider
        .send_transaction(tx)
        .await?
        .expect_successful_receipt()
        .await
}

async fn wait_for_node_last_applied_index_at_or_above(
    cluster: &MultiNodeTester,
    index: usize,
    min_index: u64,
    timeout: Duration,
) -> anyhow::Result<u64> {
    let deadline = Instant::now() + timeout;
    let mut last_observed = None;
    let mut last_error = None;
    while Instant::now() < deadline {
        match raft_status(cluster, index).await {
            Ok(status) => {
                last_observed = raft_last_applied(&status, index)?;
                if let Some(last_applied) = last_observed
                    && last_applied >= min_index
                {
                    return Ok(last_applied);
                }
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    anyhow::bail!(
        "timed out waiting for node {index} to reach last_applied_index >= {min_index}: last_observed={last_observed:?}, last_error={last_error:?}"
    )
}

async fn active_max_last_applied_index(cluster: &MultiNodeTester) -> anyhow::Result<u64> {
    let mut max_last_applied = 0;
    for idx in 0..cluster.len() {
        if cluster.is_node_suspended(idx) {
            continue;
        }
        max_last_applied = max_last_applied
            .max(raft_last_applied(&raft_status(cluster, idx).await?, idx)?.unwrap_or(0));
    }
    Ok(max_last_applied)
}

async fn send_transfer_and_wait_for_active_replication(
    cluster: &mut MultiNodeTester,
    leader_index: usize,
) -> anyhow::Result<u64> {
    let initial_applied = active_max_last_applied_index(cluster).await?;
    let receipt = send_transfer(cluster, leader_index).await?;
    let replicated_applied = cluster
        .wait_for_active_last_applied_index_convergence(initial_applied + 1, REPLICATION_TIMEOUT)
        .await?;
    let block_number = receipt
        .block_number
        .context("transfer receipt did not include a block number")?;
    wait_for_l1_finalization_if_batcher_active(cluster, block_number).await?;
    Ok(replicated_applied)
}

async fn wait_for_l1_finalization_if_batcher_active(
    cluster: &MultiNodeTester,
    block_number: u64,
) -> anyhow::Result<u64> {
    let batcher_idx = cluster.batcher_node_index();
    if cluster.is_node_suspended(batcher_idx) {
        tracing::info!(
            block_number,
            batcher_idx,
            "skipping L1 finalization check because the batcher node is suspended"
        );
        return Ok(block_number);
    }

    cluster
        .node(batcher_idx)
        .l2_zk_provider
        .wait_finalized_with_timeout(block_number, L1_FINALIZATION_TIMEOUT)
        .await
        .with_context(|| {
            format!(
                "block {block_number} was not finalized while batcher node {batcher_idx} was active"
            )
        })?;
    Ok(block_number)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_includes_simple_transaction_with_wait() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(1))
        .build()
        .await?;
    let result = async {
        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let receipt = send_transfer(&cluster, leader_index).await?;
        let block_number = receipt
            .block_number
            .context("transfer receipt did not include a block number")?;
        wait_for_l1_finalization_if_batcher_active(&cluster, block_number).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_can_be_reenabled_after_clearing_raft_history() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(1))
        .build()
        .await?;
    let result = async {
        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer(&cluster, leader_index).await?;
        send_transfer(&cluster, leader_index).await?;

        cluster.suspend_node(leader_index).await?;

        // This creates a WAL/raft gap: the restarted node clears raft history, then
        // produces a block through loopback consensus while raft is disabled.
        cluster
            .start_node_with_overrides(leader_index, |config| {
                config.consensus_config.enabled = false;
                config.consensus_config.force_clear_raft_history = true;
            })
            .await?;

        send_transfer(&cluster, leader_index).await?;

        cluster.suspend_node(leader_index).await?;

        // Re-enable consensus after the gap. The old WAL blocks are replayed locally;
        // new blocks should be raft-canonized from this point onward.
        cluster
            .start_node_with_overrides(leader_index, |config| {
                config.consensus_config.enabled = true;
                config.consensus_config.force_clear_raft_history = false;
            })
            .await?;

        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer(&cluster, leader_index).await?;

        // Restart once more with consensus enabled to verify the sparse raft history
        // written after re-enable is loadable.
        cluster.suspend_node(leader_index).await?;
        cluster.start_node(leader_index).await?;

        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer(&cluster, leader_index).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_forms_with_three_nodes_and_replicates_blocks() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_index = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let mut initial_applied = 0;
        for idx in 0..cluster.len() {
            initial_applied = initial_applied
                .max(raft_last_applied(&raft_status(&cluster, idx).await?, idx)?.unwrap_or(0));
        }

        let replicated_applied =
            send_transfer_and_wait_for_active_replication(&mut cluster, leader_index).await?;
        assert!(replicated_applied > initial_applied);

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_rotates_leader_after_failure() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let initial_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let initial_leader_node_id = raft_node_id(
            &raft_status(&cluster, initial_leader_idx).await?,
            initial_leader_idx,
        )?;

        // Warm up follower replication before taking the leader down so the surviving
        // nodes have already exchanged append entries with the elected leader.
        send_transfer_and_wait_for_active_replication(&mut cluster, initial_leader_idx).await?;

        cluster.suspend_node(initial_leader_idx).await?;

        let new_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let new_leader_id = raft_node_id(
            &raft_status(&cluster, new_leader_idx).await?,
            new_leader_idx,
        )?;

        assert_ne!(initial_leader_node_id, new_leader_id);

        send_transfer_and_wait_for_active_replication(&mut cluster, new_leader_idx).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_stops_making_progress_without_quorum() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let committed_applied =
            send_transfer_and_wait_for_active_replication(&mut cluster,leader_idx).await?;
        let follower_indices: Vec<_> = (0..cluster.len())
            .filter(|idx| *idx != leader_idx)
            .collect();
        let survivor_idx = follower_indices[1];

        cluster.suspend_node(leader_idx).await?;
        cluster.suspend_node(follower_indices[0]).await?;

        let survivor_applied =
            raft_last_applied(&raft_status(&cluster, survivor_idx).await?, survivor_idx)?
                .unwrap_or(0);
        sleep(Duration::from_secs(2)).await;
        let survivor_applied_after_wait =
            raft_last_applied(&raft_status(&cluster, survivor_idx).await?, survivor_idx)?
                .unwrap_or(0);
        assert!(
            survivor_applied <= committed_applied,
            "last_applied unexpectedly advanced before quorum-loss check: committed={committed_applied} survivor={survivor_applied}"
        );
        assert!(
            survivor_applied_after_wait <= survivor_applied,
            "last_applied unexpectedly advanced after quorum loss: before={survivor_applied} after={survivor_applied_after_wait}"
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_original_leader_rejoins_and_cluster_remains_stable() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let initial_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer_and_wait_for_active_replication(&mut cluster, initial_leader_idx).await?;

        cluster.suspend_node(initial_leader_idx).await?;

        let new_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        // Advance the cluster while the original leader is absent so it has entries to catch up.
        send_transfer_and_wait_for_active_replication(&mut cluster, new_leader_idx).await?;
        let target_applied = active_max_last_applied_index(&cluster).await?;

        // Restart the original leader. It must rejoin without disrupting the running cluster:
        // exactly one leader must remain, all three nodes must agree, and state must converge.
        cluster.start_node(initial_leader_idx).await?;
        let final_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        cluster
            .wait_for_active_last_applied_index_convergence(target_applied, REPLICATION_TIMEOUT)
            .await?;

        // Verify the cluster continues to make progress after the rejoin.
        send_transfer_and_wait_for_active_replication(&mut cluster, final_leader_idx).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_recovers_after_quorum_loss() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let committed_applied =
            send_transfer_and_wait_for_active_replication(&mut cluster,leader_idx).await?;

        let follower_indices: Vec<_> = (0..cluster.len())
            .filter(|&idx| idx != leader_idx)
            .collect();
        let survivor_idx = follower_indices[1];

        cluster.suspend_node(leader_idx).await?;
        cluster.suspend_node(follower_indices[0]).await?;

        // Verify that quorum loss stops all progress.
        let survivor_before =
            raft_last_applied(&raft_status(&cluster, survivor_idx).await?, survivor_idx)?
                .unwrap_or(0);
        sleep(Duration::from_secs(2)).await;
        let survivor_after =
            raft_last_applied(&raft_status(&cluster, survivor_idx).await?, survivor_idx)?
                .unwrap_or(0);
        assert!(
            survivor_after <= survivor_before,
            "last_applied must not advance without quorum: before={survivor_before} after={survivor_after}",
        );

        // Restore quorum and verify the cluster recovers and makes progress.
        cluster.start_node(leader_idx).await?;
        cluster.start_node(follower_indices[0]).await?;
        let new_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let recovery_applied =
            send_transfer_and_wait_for_active_replication(&mut cluster,new_leader_idx).await?;
        assert!(
            recovery_applied > committed_applied,
            "cluster must make progress after quorum is restored: committed={committed_applied} recovery={recovery_applied}",
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_fully_restarts_and_recovers() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let last_applied =
            send_transfer_and_wait_for_active_replication(&mut cluster, leader_idx).await?;

        // Suspend all nodes: state is durably on disk before any restarts.
        for idx in 0..cluster.len() {
            cluster.suspend_node(idx).await?;
        }
        // Restart all nodes: they recover from disk, re-elect a leader, and resume.
        for idx in 0..cluster.len() {
            cluster.start_node(idx).await?;
        }

        let new_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        cluster
            .wait_for_active_last_applied_index_convergence(last_applied, REPLICATION_TIMEOUT)
            .await?;

        // Verify the cluster continues to make progress after the full restart.
        send_transfer_and_wait_for_active_replication(&mut cluster, new_leader_idx).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_late_node_joins_and_catches_up() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        // Suspend the third node before cluster formation so it hasn't participated in any
        // block production — simulating a node that joins an already-established cluster.
        let late_node_idx = 2;
        cluster.suspend_node(late_node_idx).await?;

        // Two of three nodes form a quorum; the cluster must elect a leader and make progress.
        let leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer_and_wait_for_active_replication(&mut cluster, leader_idx).await?;
        send_transfer_and_wait_for_active_replication(&mut cluster, leader_idx).await?;
        let target_applied = active_max_last_applied_index(&cluster).await?;

        // Start the late node. It must receive all missed entries via Raft log replication.
        cluster.start_node(late_node_idx).await?;
        wait_for_node_last_applied_index_at_or_above(
            &cluster,
            late_node_idx,
            target_applied,
            REPLICATION_TIMEOUT,
        )
        .await?;

        // The full 3-node cluster must be stable after the late joiner has caught up.
        cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_follower_restarts_and_catches_up() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let follower_idx = (0..cluster.len())
            .find(|idx| *idx != leader_idx)
            .expect("3-node cluster must have a follower");

        cluster.suspend_node(follower_idx).await?;
        let active_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        send_transfer_and_wait_for_active_replication(&mut cluster, active_leader_idx).await?;
        let target_applied =
            send_transfer_and_wait_for_active_replication(&mut cluster, active_leader_idx).await?;

        cluster.start_node(follower_idx).await?;
        wait_for_node_last_applied_index_at_or_above(
            &cluster,
            follower_idx,
            target_applied,
            REPLICATION_TIMEOUT,
        )
        .await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}
