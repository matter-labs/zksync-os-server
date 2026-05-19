use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Context as _;
use std::time::Duration;
use tokio::time::sleep;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::provider::ZksyncTestingProvider;

const CLUSTER_FORMATION_TIMEOUT: Duration = Duration::from_secs(20);
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(20);
const L1_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(60);

mod restarted_node_catchup;

fn consensus_test_keys(n: usize) -> Vec<zksync_os_network::SecretKey> {
    (0..n)
        .map(|_| zksync_os_network::rng_secret_key())
        .collect()
}

async fn raft_node_id(cluster: &MultiNodeTester, index: usize) -> anyhow::Result<String> {
    cluster
        .node(index)
        .status()
        .await?
        .consensus
        .raft
        .map(|raft| raft.node_id)
        .ok_or_else(|| anyhow::anyhow!("node {index} did not expose raft status"))
}

async fn latest_l2_block(node: &Tester) -> anyhow::Result<u64> {
    node.l2_zk_provider
        .get_block_number_by_id(BlockId::latest())
        .await?
        .context("latest block number is missing")
}

pub(crate) async fn wait_for_l2_block(
    node: &Tester,
    block_number: u64,
    timeout: Duration,
) -> anyhow::Result<()> {
    tokio::time::timeout(timeout, node.l2_zk_provider.wait_for_block(block_number))
        .await
        .with_context(|| format!("timed out waiting for L2 block {block_number}"))??;
    Ok(())
}

async fn send_transfer(
    cluster: &MultiNodeTester,
    index: usize,
) -> anyhow::Result<alloy::rpc::types::TransactionReceipt> {
    let node = cluster.node(index);
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

/// Sends a transfer to `leader_index`, waits for all running nodes to expose the resulting
/// L2 block, then waits for L1 finalization if the batcher node is active.
/// Returns the L2 block number that included the transfer.
async fn send_transfer_and_wait_for_active_replication(
    cluster: &mut MultiNodeTester,
    leader_index: usize,
) -> anyhow::Result<u64> {
    let receipt = send_transfer(cluster, leader_index).await?;
    let block_number = receipt
        .block_number
        .context("transfer receipt did not include a block number")?;
    cluster
        .wait_for_active_l2_block(block_number, REPLICATION_TIMEOUT)
        .await?;
    wait_for_l1_finalization_if_batcher_active(cluster, block_number).await?;
    Ok(block_number)
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
        send_transfer_and_wait_for_active_replication(&mut cluster, leader_index).await?;
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
        let initial_leader_node_id = raft_node_id(&cluster, initial_leader_idx).await?;

        // Warm up follower replication before taking the leader down so the surviving
        // nodes have already exchanged append entries with the elected leader.
        send_transfer_and_wait_for_active_replication(&mut cluster, initial_leader_idx).await?;

        cluster.suspend_node(initial_leader_idx).await?;

        let new_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let new_leader_id = raft_node_id(&cluster, new_leader_idx).await?;

        assert_ne!(initial_leader_node_id, new_leader_id);

        send_transfer_and_wait_for_active_replication(&mut cluster, new_leader_idx).await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
#[ignore = "flaky; @romanbrodetski is working on it"]
async fn consensus_cluster_stops_making_progress_without_quorum() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        send_transfer_and_wait_for_active_replication(&mut cluster, leader_idx).await?;
        let follower_indices: Vec<_> = (0..cluster.len())
            .filter(|idx| *idx != leader_idx)
            .collect();
        let survivor_idx = follower_indices[1];

        cluster.suspend_node(leader_idx).await?;
        cluster.suspend_node(follower_indices[0]).await?;

        let survivor_block = latest_l2_block(cluster.node(survivor_idx)).await?;
        sleep(Duration::from_secs(2)).await;
        let survivor_block_after_wait = latest_l2_block(cluster.node(survivor_idx)).await?;
        assert_eq!(
            survivor_block_after_wait, survivor_block,
            "L2 head advanced after quorum loss: before={survivor_block} after={survivor_block_after_wait}"
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
#[ignore = "flaky; @romanbrodetski is working on it"]
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
        let target_block =
            send_transfer_and_wait_for_active_replication(&mut cluster, new_leader_idx).await?;

        // Restart the original leader. It must rejoin without disrupting the running cluster:
        // exactly one leader must remain, all three nodes must agree, and state must converge.
        cluster.start_node(initial_leader_idx).await?;
        let final_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        cluster
            .wait_for_active_l2_block(target_block, REPLICATION_TIMEOUT)
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
        let committed_block =
            send_transfer_and_wait_for_active_replication(&mut cluster, leader_idx).await?;

        let follower_indices: Vec<_> = (0..cluster.len())
            .filter(|&idx| idx != leader_idx)
            .collect();
        let survivor_idx = follower_indices[1];

        cluster.suspend_node(leader_idx).await?;
        cluster.suspend_node(follower_indices[0]).await?;

        // Verify that quorum loss stops all progress.
        let survivor_block = latest_l2_block(cluster.node(survivor_idx)).await?;
        sleep(Duration::from_secs(2)).await;
        let survivor_block_after = latest_l2_block(cluster.node(survivor_idx)).await?;
        assert_eq!(
            survivor_block_after, survivor_block,
            "L2 head must not advance without quorum: before={survivor_block} after={survivor_block_after}",
        );

        // Restore quorum and verify the cluster recovers and makes progress.
        cluster.start_node(leader_idx).await?;
        cluster.start_node(follower_indices[0]).await?;
        let new_leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let recovery_block =
            send_transfer_and_wait_for_active_replication(&mut cluster, new_leader_idx).await?;
        assert!(
            recovery_block > committed_block,
            "cluster must make progress after quorum is restored: committed={committed_block} recovery={recovery_block}",
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
#[ignore = "flaky; @romanbrodetski is working on it"]
async fn consensus_cluster_fully_restarts_and_recovers() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let last_block =
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
            .wait_for_active_l2_block(last_block, REPLICATION_TIMEOUT)
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
        let target_block =
            send_transfer_and_wait_for_active_replication(&mut cluster, leader_idx).await?;

        // Start the late node. It must receive all missed entries via Raft log replication.
        cluster.start_node(late_node_idx).await?;
        wait_for_l2_block(
            cluster.node(late_node_idx),
            target_block,
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
        let target_block =
            send_transfer_and_wait_for_active_replication(&mut cluster, active_leader_idx).await?;

        cluster.start_node(follower_idx).await?;
        wait_for_l2_block(
            cluster.node(follower_idx),
            target_block,
            REPLICATION_TIMEOUT,
        )
        .await?;

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}
