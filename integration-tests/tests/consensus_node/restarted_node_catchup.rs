use super::*;

use std::time::Duration;
use tokio::time::Instant;

const CONSENSUS_RESTART_GAP_BLOCKS: usize = 3;
const CONSENSUS_CONTINUED_BLOCKS_AFTER_RESTART: usize = 2;
const CONSENSUS_RESTART_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(120);

async fn produce_consensus_blocks(
    cluster: &mut MultiNodeTester,
    node_indices: &[usize],
    count: usize,
) -> anyhow::Result<Vec<u64>> {
    let mut blocks = Vec::with_capacity(count);
    for ordinal in 0..count {
        let block_number = send_transfer_and_wait_for_l2_blocks_eventually(
            cluster,
            node_indices,
            CONSENSUS_PROGRESS_TIMEOUT,
        )
        .await
        .with_context(|| {
            format!(
                "failed to produce consensus block {}/{} among {node_indices:?}",
                ordinal + 1,
                count
            )
        })?;
        blocks.push(block_number);
    }
    Ok(blocks)
}

async fn l2_block_snapshot(cluster: &MultiNodeTester, node_indices: &[usize]) -> Vec<String> {
    let mut snapshot = Vec::with_capacity(node_indices.len());
    for &index in node_indices {
        match latest_l2_block(cluster.node(index)).await {
            Ok(block) => snapshot.push(format!("node_{index}: block={block}")),
            Err(error) => snapshot.push(format!("node_{index}: block_error={error:#}")),
        }
    }
    snapshot
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_after_transaction_gap() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let restarted_node_idx = (0..cluster.len())
            .find(|idx| *idx != leader_idx)
            .expect("3-node cluster must have a follower");
        let active_node_indices = (0..cluster.len())
            .filter(|idx| *idx != restarted_node_idx)
            .collect::<Vec<_>>();
        let all_node_indices = (0..cluster.len()).collect::<Vec<_>>();
        let restarted_node_initial_block = latest_l2_block(cluster.node(restarted_node_idx)).await?;

        cluster.suspend_node(restarted_node_idx).await?;
        cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let started_at = Instant::now();
        let gap_blocks = produce_consensus_blocks(
            &mut cluster,
            &active_node_indices,
            CONSENSUS_RESTART_GAP_BLOCKS,
        )
        .await?;
        let target_block = *gap_blocks
            .last()
            .context("gap producer did not return any blocks")?;
        assert!(
            target_block > restarted_node_initial_block,
            "active cluster head did not advance while node was down: initial={restarted_node_initial_block}, target={target_block}"
        );

        cluster.start_node(restarted_node_idx).await?;
        let catch_up_result = wait_for_l2_block(
            cluster.node(restarted_node_idx),
            target_block,
            CONSENSUS_RESTART_CATCH_UP_TIMEOUT,
        )
        .await;
        if let Err(error) = catch_up_result {
            let final_l2_blocks = l2_block_snapshot(&cluster, &all_node_indices).await;
            return Err(error).with_context(|| {
                format!(
                    "restarted consensus node did not catch up after transaction gap: \
                     target_block={target_block}, initial_block={restarted_node_initial_block}, \
                     active_nodes={active_node_indices:?}, l2_blocks=[{}]",
                    final_l2_blocks.join(", "),
                )
            });
        }
        let caught_up_at = started_at.elapsed();

        let post_rejoin_block = send_transfer_and_wait_for_l2_blocks_eventually(
            &mut cluster,
            &all_node_indices,
            CONSENSUS_PROGRESS_TIMEOUT,
        )
        .await?;
        assert!(
            post_rejoin_block > target_block,
            "cluster did not keep producing after restarted node caught up: post_rejoin_block={post_rejoin_block}, target_block={target_block}"
        );

        tracing::info!(
            gap_blocks = gap_blocks.len(),
            first_gap_block = gap_blocks.first().copied(),
            target_block,
            caught_up_ms = caught_up_at.as_millis(),
            post_rejoin_block,
            "restarted consensus node caught up after deterministic transaction gap"
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_while_new_blocks_continue() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let restarted_node_idx = (0..cluster.len())
            .find(|idx| *idx != leader_idx)
            .expect("3-node cluster must have a follower");
        let active_node_indices = (0..cluster.len())
            .filter(|idx| *idx != restarted_node_idx)
            .collect::<Vec<_>>();
        let all_node_indices = (0..cluster.len()).collect::<Vec<_>>();
        let restarted_node_initial_block = latest_l2_block(cluster.node(restarted_node_idx)).await?;

        cluster.suspend_node(restarted_node_idx).await?;
        cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let started_at = Instant::now();
        let blocks_before_restart = produce_consensus_blocks(
            &mut cluster,
            &active_node_indices,
            CONSENSUS_RESTART_GAP_BLOCKS,
        )
        .await?;
        let target_block_at_restart = *blocks_before_restart
            .last()
            .context("pre-restart producer did not return any blocks")?;
        assert!(
            target_block_at_restart > restarted_node_initial_block,
            "active cluster head did not advance while node was down: initial={restarted_node_initial_block}, target_at_restart={target_block_at_restart}"
        );

        let restart_started_at = started_at.elapsed();
        cluster.start_node(restarted_node_idx).await?;
        let restart_completed_at = started_at.elapsed();

        let blocks_after_restart = produce_consensus_blocks(
            &mut cluster,
            &active_node_indices,
            CONSENSUS_CONTINUED_BLOCKS_AFTER_RESTART,
        )
        .await?;
        let final_active_block = *blocks_after_restart
            .last()
            .context("post-restart producer did not return any blocks")?;
        assert!(
            final_active_block > target_block_at_restart,
            "active cluster did not keep producing after restart: final_active_block={final_active_block}, target_at_restart={target_block_at_restart}"
        );

        let catch_up_result = wait_for_l2_block(
            cluster.node(restarted_node_idx),
            final_active_block,
            CONSENSUS_RESTART_CATCH_UP_TIMEOUT,
        )
        .await;
        if let Err(error) = catch_up_result {
            let final_l2_blocks = l2_block_snapshot(&cluster, &all_node_indices).await;
            return Err(error).with_context(|| {
                format!(
                    "restarted consensus node did not catch up to final active block: \
                     final_active_block={final_active_block}, initial_block={restarted_node_initial_block}, \
                     active_nodes={active_node_indices:?}, l2_blocks=[{}]",
                    final_l2_blocks.join(", "),
                )
            });
        }
        let caught_up_at = started_at.elapsed();

        let post_rejoin_block = send_transfer_and_wait_for_l2_blocks_eventually(
            &mut cluster,
            &all_node_indices,
            CONSENSUS_PROGRESS_TIMEOUT,
        )
        .await?;
        assert!(
            post_rejoin_block > final_active_block,
            "cluster did not keep producing after restarted node caught up: post_rejoin_block={post_rejoin_block}, final_active_block={final_active_block}"
        );

        tracing::info!(
            blocks_before_restart = blocks_before_restart.len(),
            blocks_after_restart = blocks_after_restart.len(),
            first_block_before_restart = blocks_before_restart.first().copied(),
            target_block_at_restart,
            final_active_block,
            restart_started_ms = restart_started_at.as_millis(),
            restart_completed_ms = restart_completed_at.as_millis(),
            caught_up_ms = caught_up_at.as_millis(),
            post_rejoin_block,
            "restarted consensus node caught up while active nodes continued producing"
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}
