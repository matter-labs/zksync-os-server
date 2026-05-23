use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Context as _;
use std::time::Duration;
use tokio::time::Instant;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::provider::ZksyncTestingProvider;

const CLUSTER_FORMATION_TIMEOUT: Duration = Duration::from_secs(20);
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(20);
const L1_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(60);
const CONSENSUS_PROGRESS_TIMEOUT: Duration = Duration::from_secs(90);
const CONSENSUS_TRANSFER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
const L2_RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const NO_QUORUM_PROGRESS_TIMEOUT: Duration = Duration::from_secs(5);

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
    tokio::time::timeout(
        L2_RPC_REQUEST_TIMEOUT,
        node.l2_zk_provider
            .get_block_number_by_id(BlockId::latest()),
    )
    .await
    .context("timed out fetching latest L2 block")??
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
    node.wait_for_initial_deposit()
        .await
        .with_context(|| format!("node {index} did not become ready for L2 transfers"))?;

    tokio::time::timeout(CONSENSUS_TRANSFER_ATTEMPT_TIMEOUT, async {
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
    })
    .await
    .with_context(|| format!("timed out sending transfer through node {index}"))?
    .with_context(|| format!("failed sending transfer through node {index}"))
}

pub(crate) async fn send_transfer_and_wait_for_l2_blocks(
    cluster: &MultiNodeTester,
    leader_index: usize,
    node_indices: &[usize],
) -> anyhow::Result<u64> {
    let receipt = send_transfer(cluster, leader_index).await?;
    let block_number = receipt
        .block_number
        .context("transfer receipt did not include a block number")?;
    for &index in node_indices {
        wait_for_l2_block(cluster.node(index), block_number, REPLICATION_TIMEOUT)
            .await
            .with_context(|| format!("node {index} did not reach L2 block {block_number}"))?;
    }
    Ok(block_number)
}

pub(crate) async fn send_transfer_and_wait_for_l2_blocks_eventually(
    cluster: &mut MultiNodeTester,
    node_indices: &[usize],
    timeout: Duration,
) -> anyhow::Result<u64> {
    anyhow::ensure!(
        !node_indices.is_empty(),
        "cannot produce a consensus block with an empty node set"
    );

    let deadline = Instant::now() + timeout;
    let mut attempts = 0;
    let mut last_error = None;

    while Instant::now() < deadline {
        let formation_timeout =
            CLUSTER_FORMATION_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
        let leader_index = match cluster
            .wait_for_raft_cluster_formation_among(node_indices, formation_timeout)
            .await
        {
            Ok(leader_index) => leader_index,
            Err(error) => {
                last_error = Some(format!("cluster formation failed: {error:#}"));
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };

        attempts += 1;
        let attempt_timeout = deadline.saturating_duration_since(Instant::now());
        if attempt_timeout.is_zero() {
            break;
        }

        match tokio::time::timeout(
            attempt_timeout,
            send_transfer_and_wait_for_l2_blocks(cluster, leader_index, node_indices),
        )
        .await
        {
            Ok(Ok(block_number)) => return Ok(block_number),
            Ok(Err(error)) => {
                tracing::warn!(
                    attempts,
                    leader_index,
                    error = %error,
                    "consensus transfer attempt failed; retrying"
                );
                last_error = Some(error.to_string());
            }
            Err(_) => {
                tracing::warn!(
                    attempts,
                    leader_index,
                    timeout_ms = attempt_timeout.as_millis(),
                    "consensus transfer attempt timed out; retrying"
                );
                last_error = Some(format!(
                    "transfer attempt timed out after {attempt_timeout:?}"
                ));
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    anyhow::bail!(
        "timed out producing a consensus block among {node_indices:?}: attempts={attempts}, last_error={last_error:?}"
    )
}

async fn assert_no_transaction_progress_without_quorum(
    cluster: &MultiNodeTester,
    survivor_idx: usize,
) -> anyhow::Result<()> {
    let survivor_block = latest_l2_block(cluster.node(survivor_idx)).await?;
    match tokio::time::timeout(
        NO_QUORUM_PROGRESS_TIMEOUT,
        send_transfer(cluster, survivor_idx),
    )
    .await
    {
        Ok(Ok(receipt)) => {
            anyhow::bail!(
                "transaction unexpectedly reached a receipt without quorum: survivor_idx={survivor_idx}, receipt_block={:?}",
                receipt.block_number
            );
        }
        Ok(Err(error)) => {
            tracing::info!(
                survivor_idx,
                error = %error,
                "transaction failed without quorum as expected"
            );
        }
        Err(_) => {
            tracing::info!(
                survivor_idx,
                timeout_ms = NO_QUORUM_PROGRESS_TIMEOUT.as_millis(),
                "transaction did not reach a receipt without quorum as expected"
            );
        }
    }

    let survivor_block_after = latest_l2_block(cluster.node(survivor_idx)).await?;
    anyhow::ensure!(
        survivor_block_after == survivor_block,
        "L2 head advanced after quorum loss: before={survivor_block} after={survivor_block_after}"
    );
    Ok(())
}

/// Sends a transfer to `submit_index`, waits for all running nodes to expose the resulting
/// L2 block, then waits for L1 finalization if the batcher node is active.
/// Returns the L2 block number that included the transfer.
async fn send_transfer_and_wait_for_active_replication(
    cluster: &mut MultiNodeTester,
    submit_index: usize,
) -> anyhow::Result<u64> {
    let receipt = send_transfer(cluster, submit_index).await?;
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
async fn consensus_cluster_accepts_transactions_from_any_node() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        for node_index in 0..cluster.len() {
            send_transfer_and_wait_for_active_replication(&mut cluster, node_index)
                .await
                .with_context(|| format!("transaction submitted to node {node_index} failed"))?;
        }

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
async fn consensus_cluster_stops_making_progress_without_quorum() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let all_node_indices = (0..cluster.len()).collect::<Vec<_>>();
        send_transfer_and_wait_for_l2_blocks_eventually(
            &mut cluster,
            &all_node_indices,
            CONSENSUS_PROGRESS_TIMEOUT,
        )
        .await?;
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let follower_indices: Vec<_> = (0..cluster.len())
            .filter(|idx| *idx != leader_idx)
            .collect();
        let survivor_idx = leader_idx;

        cluster.suspend_node(follower_indices[0]).await?;
        cluster.suspend_node(follower_indices[1]).await?;

        assert_no_transaction_progress_without_quorum(&cluster, survivor_idx).await?;

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
        let target_block =
            send_transfer_and_wait_for_active_replication(&mut cluster, new_leader_idx).await?;

        // Restart the original leader. It must rejoin without disrupting the running cluster:
        // exactly one leader must remain, all three nodes must agree, and state must converge.
        cluster.start_node(initial_leader_idx).await?;
        cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        cluster
            .wait_for_active_l2_block(target_block, REPLICATION_TIMEOUT)
            .await?;

        // Verify the cluster continues to make progress after the rejoin. Re-check the current
        // leader inside the retry loop because leadership can still settle immediately after the
        // restarted node catches up.
        let all_node_indices = (0..cluster.len()).collect::<Vec<_>>();
        let progress_block = send_transfer_and_wait_for_l2_blocks_eventually(
            &mut cluster,
            &all_node_indices,
            CONSENSUS_PROGRESS_TIMEOUT,
        )
        .await?;
        wait_for_l1_finalization_if_batcher_active(&cluster, progress_block).await?;

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
        let all_node_indices = (0..cluster.len()).collect::<Vec<_>>();
        let committed_block = send_transfer_and_wait_for_l2_blocks_eventually(
            &mut cluster,
            &all_node_indices,
            CONSENSUS_PROGRESS_TIMEOUT,
        )
        .await?;
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let follower_indices: Vec<_> = (0..cluster.len())
            .filter(|&idx| idx != leader_idx)
            .collect();
        let survivor_idx = leader_idx;

        cluster.suspend_node(follower_indices[0]).await?;
        cluster.suspend_node(follower_indices[1]).await?;

        // Verify that quorum loss stops all progress.
        assert_no_transaction_progress_without_quorum(&cluster, survivor_idx).await?;

        // Restore quorum and verify the cluster recovers and makes progress.
        cluster.start_node(follower_indices[0]).await?;
        let recovered_node_indices = [leader_idx, follower_indices[0]];
        let recovery_block = send_transfer_and_wait_for_l2_blocks_eventually(
            &mut cluster,
            &recovered_node_indices,
            CONSENSUS_PROGRESS_TIMEOUT,
        )
        .await?;
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
        for idx in (0..cluster.len()).rev() {
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
