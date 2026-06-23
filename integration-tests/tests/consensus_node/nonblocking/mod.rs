//! Consensus tests that run in the non-required CI lane. These exercise
//! leader-change / restart / quorum-loss / tx-forwarding scenarios that are flaky under
//! the test environment's RLPx simultaneous-dial artifact (a reth session-manager bug),
//! not in production. They are real scenario tests, run every PR but not merge-blocking.

mod restarted_node_catchup;

use crate::consensus_node::{
    CLUSTER_FORMATION_TIMEOUT, REPLICATION_TIMEOUT, TransferSubmission,
    assert_no_progress_without_quorum, send_transfer_and_replicate, send_transfer_with,
};
use anyhow::Context as _;
use zksync_os_integration_tests::multi_node::ConsensusCluster;

#[test_log::test(tokio::test)]
async fn consensus_cluster_rotates_leader_after_failure() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let initial_leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let initial_leader_node_id = cluster.raft_node_id(initial_leader).await?;

        // Warm up follower replication before taking the leader down so survivors have
        // already exchanged append entries with the elected leader.
        send_transfer_and_replicate(&cluster, initial_leader).await?;

        cluster.suspend(initial_leader).await?;

        let new_leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let new_leader_node_id = cluster.raft_node_id(new_leader).await?;
        assert_ne!(initial_leader_node_id, new_leader_node_id);

        send_transfer_and_replicate(&cluster, new_leader).await?;
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_stops_making_progress_without_quorum() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        send_transfer_and_replicate(&cluster, leader).await?;

        let followers: Vec<_> = cluster
            .live_indices()
            .into_iter()
            .filter(|idx| *idx != leader)
            .collect();
        cluster.suspend(followers[0]).await?;
        cluster.suspend(followers[1]).await?;

        assert_no_progress_without_quorum(&cluster, leader).await?;
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_original_leader_rejoins_and_cluster_remains_stable() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let initial_leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        send_transfer_and_replicate(&cluster, initial_leader).await?;

        cluster.suspend(initial_leader).await?;
        let new_leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;

        // Advance the cluster while the original leader is absent so it has entries to
        // catch up.
        let target_block = send_transfer_and_replicate(&cluster, new_leader).await?;

        // Restart the original leader. It must rejoin without disrupting the running
        // cluster: exactly one leader remains, all three nodes agree, and state converges.
        cluster.start(initial_leader).await?;
        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        cluster
            .wait_replicated(target_block, REPLICATION_TIMEOUT)
            .await?;

        // Verify the cluster keeps making progress after the rejoin.
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        send_transfer_and_replicate(&cluster, leader).await?;
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_fully_restarts_and_recovers() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let last_block = send_transfer_and_replicate(&cluster, leader).await?;

        // Suspend all nodes: state is durably on disk before any restarts.
        for idx in cluster.indices() {
            cluster.suspend(idx).await?;
        }
        // Restart all nodes: they recover from disk, re-elect a leader, and resume.
        for idx in cluster.indices() {
            cluster.start(idx).await?;
        }

        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        cluster
            .wait_replicated(last_block, REPLICATION_TIMEOUT)
            .await?;
        // Verify the cluster continues to make progress after the full restart.
        send_transfer_and_replicate(&cluster, leader).await?;
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_late_node_joins_and_catches_up() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        // Suspend the third node before cluster formation so it hasn't participated in
        // any block production — simulating a node that joins an already-established
        // cluster.
        let late_node = 2;
        cluster.suspend(late_node).await?;

        // Two of three nodes form a quorum; the cluster elects a leader and makes progress.
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        send_transfer_and_replicate(&cluster, leader).await?;
        let target_block = send_transfer_and_replicate(&cluster, leader).await?;

        // Start the late node. It must receive all missed entries via raft replication.
        cluster.start(late_node).await?;
        cluster
            .wait_node_at(late_node, target_block, REPLICATION_TIMEOUT)
            .await?;

        // The full 3-node cluster must be stable after the late joiner caught up.
        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_follower_restarts_and_catches_up() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let follower = cluster
            .live_indices()
            .into_iter()
            .find(|idx| *idx != leader)
            .expect("3-node cluster must have a follower");

        cluster.suspend(follower).await?;
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;

        send_transfer_and_replicate(&cluster, leader).await?;
        let target_block = send_transfer_and_replicate(&cluster, leader).await?;

        cluster.start(follower).await?;
        cluster
            .wait_node_at(follower, target_block, REPLICATION_TIMEOUT)
            .await?;
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

/// Verifies the cluster accepts transactions submitted via any node (not just the leader),
/// exercising the tx-forwarding path from PR #1321.
#[test_log::test(tokio::test)]
async fn consensus_cluster_accepts_transactions_from_any_node() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        for node_index in cluster.live_indices() {
            let block =
                send_transfer_with(&cluster, node_index, TransferSubmission::SendTransaction)
                    .await
                    .with_context(|| {
                        format!("transaction submitted to node {node_index} failed")
                    })?;
            cluster.wait_replicated(block, REPLICATION_TIMEOUT).await?;
        }
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

/// `eth_sendRawTransactionSync` must succeed from both leader and replica.
#[test_log::test(tokio::test)]
async fn consensus_cluster_send_raw_transaction_sync_accepts_leader_and_replica()
-> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let replica = cluster
            .live_indices()
            .into_iter()
            .find(|idx| *idx != leader)
            .context("3-node cluster must have a replica")?;

        let leader_block =
            send_transfer_with(&cluster, leader, TransferSubmission::SendRawTransactionSync)
                .await
                .with_context(|| {
                    format!("eth_sendRawTransactionSync via leader {leader} failed")
                })?;
        cluster
            .wait_replicated(leader_block, REPLICATION_TIMEOUT)
            .await?;

        let replica_block = send_transfer_with(
            &cluster,
            replica,
            TransferSubmission::SendRawTransactionSync,
        )
        .await
        .with_context(|| format!("eth_sendRawTransactionSync via replica {replica} failed"))?;
        cluster
            .wait_replicated(replica_block, REPLICATION_TIMEOUT)
            .await?;

        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}
