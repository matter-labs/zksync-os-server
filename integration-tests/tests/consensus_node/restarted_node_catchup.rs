use super::*;

use alloy::eips::BlockId;
use alloy::providers::Provider;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
use zksync_os_integration_tests::rpc_recorder::{HttpRpcRecorder, HttpRpcReport, RpcRecordConfig};

const CONSENSUS_LONG_GAP_LOAD_DURATION: Duration = Duration::from_secs(60);
const CONSENSUS_CONTINUED_LOAD_AFTER_RESTART_DURATION: Duration = Duration::from_secs(45);
const CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(180);
const MIN_LONG_GAP_LOAD_BLOCKS: usize = 10;
const MIN_CONTINUED_LOAD_BLOCKS: usize = 5;

struct ConsensusLoadStats {
    attempts: usize,
    blocks: Vec<u64>,
    elapsed: Duration,
}

struct ConsensusRejoinLoadStats {
    attempts: usize,
    blocks: Vec<u64>,
    blocks_before_restart: usize,
    blocks_after_restart: usize,
    elapsed: Duration,
    restart_started_at: Duration,
    restart_completed_at: Duration,
    target_block_at_restart: u64,
    target_applied_at_restart: u64,
    raft_caught_up_index: u64,
    raft_caught_up_at: Duration,
    l2_caught_up_at: Duration,
    rpc_caught_up_at: Duration,
}

async fn latest_l2_block(node: &Tester) -> anyhow::Result<u64> {
    node.l2_zk_provider
        .get_block_number_by_id(BlockId::latest())
        .await?
        .context("latest block number is missing")
}

async fn wait_for_l2_block(
    node: &Tester,
    block_number: u64,
    timeout: Duration,
) -> anyhow::Result<()> {
    tokio::time::timeout(timeout, node.l2_zk_provider.wait_for_block(block_number))
        .await
        .with_context(|| format!("timed out waiting for L2 block {block_number}"))??;
    Ok(())
}

async fn send_transfer_and_wait_for_l2_blocks(
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

fn remaining_storm_send_timeout(deadline: Instant) -> Option<Duration> {
    let now = Instant::now();
    if now >= deadline {
        return None;
    }

    Some((REPLICATION_TIMEOUT + Duration::from_secs(10)).min(deadline.duration_since(now)))
}

async fn generate_consensus_transaction_storm(
    cluster: &mut MultiNodeTester,
    node_indices: &[usize],
    duration: Duration,
) -> anyhow::Result<ConsensusLoadStats> {
    let started_at = Instant::now();
    let deadline = started_at + duration;
    let mut attempts = 0;
    let mut blocks = Vec::new();
    let mut last_error = None;

    while Instant::now() < deadline {
        let leader_index = cluster
            .wait_for_raft_cluster_formation_among(node_indices, CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let Some(send_timeout) = remaining_storm_send_timeout(deadline) else {
            break;
        };
        attempts += 1;

        match tokio::time::timeout(
            send_timeout,
            send_transfer_and_wait_for_l2_blocks(cluster, leader_index, node_indices),
        )
        .await
        {
            Ok(Ok(block_number)) => {
                tracing::info!(
                    attempts,
                    produced_blocks = blocks.len() + 1,
                    block_number,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "consensus transaction storm produced a block"
                );
                blocks.push(block_number);
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    attempts,
                    error = %error,
                    "consensus transaction storm send failed; retrying"
                );
                last_error = Some(error.to_string());
                sleep(Duration::from_millis(200)).await;
            }
            Err(_) => {
                tracing::warn!(
                    attempts,
                    "consensus transaction storm send timed out; retrying"
                );
                last_error = Some("timed out sending transfer".to_owned());
                sleep(Duration::from_millis(200)).await;
            }
        }
    }

    anyhow::ensure!(
        blocks.len() >= MIN_LONG_GAP_LOAD_BLOCKS,
        "transaction storm produced too few blocks: produced={}, attempts={}, last_error={:?}",
        blocks.len(),
        attempts,
        last_error
    );

    Ok(ConsensusLoadStats {
        attempts,
        blocks,
        elapsed: started_at.elapsed(),
    })
}

async fn node_last_applied_index(
    cluster: &MultiNodeTester,
    index: usize,
) -> anyhow::Result<Option<u64>> {
    raft_last_applied(&raft_status(cluster, index).await?, index)
}

async fn observe_restarted_node_catch_up_to_target(
    cluster: &MultiNodeTester,
    restarted_node_idx: usize,
    restarted_rpc_monitor: &HttpRpcRecorder,
    target_block: u64,
    target_applied: u64,
    started_at: Instant,
    raft_caught_up: &mut Option<(u64, Duration)>,
    l2_caught_up: &mut Option<Duration>,
    rpc_caught_up: &mut Option<Duration>,
) {
    if raft_caught_up.is_none()
        && let Ok(Some(last_applied)) = node_last_applied_index(cluster, restarted_node_idx).await
        && last_applied >= target_applied
    {
        *raft_caught_up = Some((last_applied, started_at.elapsed()));
    }

    if l2_caught_up.is_none()
        && let Ok(latest_block) = latest_l2_block(cluster.node(restarted_node_idx)).await
        && latest_block >= target_block
    {
        *l2_caught_up = Some(started_at.elapsed());
    }

    if rpc_caught_up.is_none()
        && restarted_rpc_monitor
            .first_observed_block_at(target_block)
            .await
            .is_some()
    {
        *rpc_caught_up = Some(started_at.elapsed());
    }
}

async fn generate_consensus_transaction_storm_across_restart(
    cluster: &mut MultiNodeTester,
    active_node_indices: &[usize],
    restarted_node_idx: usize,
    restart_after: Duration,
    continue_after_restart: Duration,
    restarted_rpc_monitor: &HttpRpcRecorder,
) -> anyhow::Result<ConsensusRejoinLoadStats> {
    let started_at = Instant::now();
    let restart_deadline = started_at + restart_after;
    let stop_deadline = restart_deadline + continue_after_restart;
    let mut attempts = 0;
    let mut blocks = Vec::new();
    let mut blocks_before_restart = 0;
    let mut blocks_after_restart = 0;
    let mut restarted = false;
    let mut restart_started_at = None;
    let mut restart_completed_at = None;
    let mut target_block_at_restart = None;
    let mut target_applied_at_restart = None;
    let mut raft_caught_up = None;
    let mut l2_caught_up = None;
    let mut rpc_caught_up = None;
    let mut last_error = None;

    while Instant::now() < stop_deadline {
        if !restarted && Instant::now() >= restart_deadline {
            let target_block = latest_l2_block(cluster.node(active_node_indices[0])).await?;
            let target_applied = active_max_last_applied_index(cluster).await?;
            let started = started_at.elapsed();
            tracing::info!(
                restarted_node_idx,
                target_block,
                target_applied,
                elapsed_ms = started.as_millis(),
                "restarting consensus node while transaction storm continues"
            );
            cluster.start_node(restarted_node_idx).await?;
            let completed = started_at.elapsed();

            restarted = true;
            restart_started_at = Some(started);
            restart_completed_at = Some(completed);
            target_block_at_restart = Some(target_block);
            target_applied_at_restart = Some(target_applied);

            observe_restarted_node_catch_up_to_target(
                cluster,
                restarted_node_idx,
                restarted_rpc_monitor,
                target_block,
                target_applied,
                started_at,
                &mut raft_caught_up,
                &mut l2_caught_up,
                &mut rpc_caught_up,
            )
            .await;
        }

        if restarted {
            observe_restarted_node_catch_up_to_target(
                cluster,
                restarted_node_idx,
                restarted_rpc_monitor,
                target_block_at_restart.expect("target block set after restart"),
                target_applied_at_restart.expect("target applied set after restart"),
                started_at,
                &mut raft_caught_up,
                &mut l2_caught_up,
                &mut rpc_caught_up,
            )
            .await;
        }

        let leader_index = cluster
            .wait_for_raft_cluster_formation_among(active_node_indices, CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let Some(send_timeout) = remaining_storm_send_timeout(stop_deadline) else {
            break;
        };
        attempts += 1;

        match tokio::time::timeout(
            send_timeout,
            send_transfer_and_wait_for_l2_blocks(cluster, leader_index, active_node_indices),
        )
        .await
        {
            Ok(Ok(block_number)) => {
                if restarted {
                    blocks_after_restart += 1;
                } else {
                    blocks_before_restart += 1;
                }
                tracing::info!(
                    attempts,
                    produced_blocks = blocks.len() + 1,
                    blocks_before_restart,
                    blocks_after_restart,
                    block_number,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "continuous consensus transaction storm produced a block"
                );
                blocks.push(block_number);
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    attempts,
                    error = %error,
                    "continuous consensus transaction storm send failed; retrying"
                );
                last_error = Some(error.to_string());
                sleep(Duration::from_millis(200)).await;
            }
            Err(_) => {
                tracing::warn!(
                    attempts,
                    "continuous consensus transaction storm send timed out; retrying"
                );
                last_error = Some("timed out sending transfer".to_owned());
                sleep(Duration::from_millis(200)).await;
            }
        }

        if restarted {
            observe_restarted_node_catch_up_to_target(
                cluster,
                restarted_node_idx,
                restarted_rpc_monitor,
                target_block_at_restart.expect("target block set after restart"),
                target_applied_at_restart.expect("target applied set after restart"),
                started_at,
                &mut raft_caught_up,
                &mut l2_caught_up,
                &mut rpc_caught_up,
            )
            .await;
        }
    }

    if restarted {
        observe_restarted_node_catch_up_to_target(
            cluster,
            restarted_node_idx,
            restarted_rpc_monitor,
            target_block_at_restart.expect("target block set after restart"),
            target_applied_at_restart.expect("target applied set after restart"),
            started_at,
            &mut raft_caught_up,
            &mut l2_caught_up,
            &mut rpc_caught_up,
        )
        .await;
    }

    anyhow::ensure!(
        restarted,
        "transaction storm ended before restarting node {restarted_node_idx}"
    );
    anyhow::ensure!(
        blocks_before_restart >= MIN_LONG_GAP_LOAD_BLOCKS,
        "transaction storm produced too few blocks before restart: produced={}, attempts={}, last_error={:?}",
        blocks_before_restart,
        attempts,
        last_error
    );
    anyhow::ensure!(
        blocks_after_restart >= MIN_CONTINUED_LOAD_BLOCKS,
        "transaction storm produced too few blocks after restart: produced={}, attempts={}, last_error={:?}",
        blocks_after_restart,
        attempts,
        last_error
    );

    let (raft_caught_up_index, raft_caught_up_at) = raft_caught_up
        .context("restarted node did not Raft-apply the restart target while load continued")?;
    let l2_caught_up_at = l2_caught_up.context(
        "restarted node did not expose the restart target L2 block while load continued",
    )?;
    let rpc_caught_up_at = rpc_caught_up
        .context("RPC monitor did not observe the restarted node reaching the restart target")?;

    Ok(ConsensusRejoinLoadStats {
        attempts,
        blocks,
        blocks_before_restart,
        blocks_after_restart,
        elapsed: started_at.elapsed(),
        restart_started_at: restart_started_at.expect("restart started"),
        restart_completed_at: restart_completed_at.expect("restart completed"),
        target_block_at_restart: target_block_at_restart.expect("target block set"),
        target_applied_at_restart: target_applied_at_restart.expect("target applied set"),
        raft_caught_up_index,
        raft_caught_up_at,
        l2_caught_up_at,
        rpc_caught_up_at,
    })
}

fn first_rpc_observed_block_at(report: &HttpRpcReport, target_block: u64) -> Option<Duration> {
    report
        .samples
        .iter()
        .find(|sample| {
            sample
                .block_number
                .is_some_and(|block| block >= target_block)
        })
        .map(|sample| sample.elapsed)
}

fn assert_rpc_monitor_stayed_ready(report: &HttpRpcReport) -> anyhow::Result<()> {
    report.assert_eventually_ready()?;
    anyhow::ensure!(
        report.error_samples() == 0,
        "{} observed RPC errors while it should have stayed ready: {report}\n{}",
        report.name,
        report.format_detailed_timeline()
    );
    Ok(())
}

fn assert_rpc_monitor_recovered_after_outage(report: &HttpRpcReport) -> anyhow::Result<()> {
    let first_error_at = report.first_error_at().with_context(|| {
        format!(
            "{} did not observe the expected RPC outage while the node was stopped: {report}",
            report.name
        )
    })?;
    let first_ready_at = report.first_ready_at().with_context(|| {
        format!(
            "{} never recovered after the expected RPC outage: {report}",
            report.name
        )
    })?;

    anyhow::ensure!(
        first_error_at < first_ready_at,
        "{} observed readiness before the expected outage: {report}\n{}",
        report.name,
        report.format_detailed_timeline()
    );
    Ok(())
}

async fn wait_for_rpc_monitor_block(
    recorder: &HttpRpcRecorder,
    target_block: u64,
    timeout: Duration,
) -> anyhow::Result<Duration> {
    let deadline = Instant::now() + timeout;
    let mut last_observed = None;

    while Instant::now() < deadline {
        if let Some(observed_at) = recorder.first_observed_block_at(target_block).await {
            return Ok(observed_at);
        }

        last_observed = recorder.latest_block_number().await;
        sleep(Duration::from_millis(50)).await;
    }

    anyhow::bail!(
        "timed out waiting for RPC monitor to observe block >= {target_block}: last_observed={last_observed:?}"
    )
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

async fn raft_last_applied_snapshot(
    cluster: &MultiNodeTester,
    node_indices: &[usize],
) -> Vec<String> {
    let mut snapshot = Vec::with_capacity(node_indices.len());
    for &index in node_indices {
        match node_last_applied_index(cluster, index).await {
            Ok(last_applied) => {
                snapshot.push(format!("node_{index}: last_applied={last_applied:?}"))
            }
            Err(error) => snapshot.push(format!("node_{index}: raft_error={error:#}")),
        }
    }
    snapshot
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_after_long_transaction_storm() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let restarted_node_idx = 2;
        let active_node_indices = (0..cluster.len())
            .filter(|idx| *idx != restarted_node_idx)
            .collect::<Vec<_>>();
        let restarted_node_rpc_url = cluster.node(restarted_node_idx).l2_rpc_url().to_owned();
        let restarted_node_initial_block = latest_l2_block(cluster.node(restarted_node_idx)).await?;

        cluster.suspend_node(restarted_node_idx).await?;
        cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let load_stats = generate_consensus_transaction_storm(
            &mut cluster,
            &active_node_indices,
            CONSENSUS_LONG_GAP_LOAD_DURATION,
        )
        .await?;
        let target_block = latest_l2_block(cluster.node(active_node_indices[0])).await?;
        let target_applied = active_max_last_applied_index(&cluster).await?;
        assert!(
            target_block > restarted_node_initial_block,
            "active cluster head did not advance while node was down: initial={restarted_node_initial_block}, target={target_block}"
        );
        tracing::info!(
            attempts = load_stats.attempts,
            tx_blocks = load_stats.blocks.len(),
            first_tx_block = load_stats.blocks.first().copied(),
            last_tx_block = load_stats.blocks.last().copied(),
            elapsed_ms = load_stats.elapsed.as_millis(),
            restarted_node_initial_block,
            target_block,
            target_applied,
            "transaction storm finished while consensus node was stopped"
        );

        let rpc_monitor = HttpRpcRecorder::start_http(
            "restarted-consensus-node",
            restarted_node_rpc_url,
            RpcRecordConfig {
                poll_interval: Duration::from_millis(200),
                request_timeout: Duration::from_secs(1),
                max_samples: 20_000,
            },
        );
        let restart_started_at = Instant::now();
        cluster.start_node(restarted_node_idx).await?;

        let catch_up_result = async {
            let raft_caught_up_index = wait_for_node_last_applied_index_at_or_above(
                &cluster,
                restarted_node_idx,
                target_applied,
                CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT,
            )
            .await
            .context("restarted node did not reach target Raft last_applied_index")?;
            let raft_caught_up_at = restart_started_at.elapsed();

            wait_for_l2_block(
                cluster.node(restarted_node_idx),
                target_block,
                CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT,
            )
            .await
            .context("restarted node Raft-applied the target but did not expose the target L2 block")?;
            let l2_caught_up_at = restart_started_at.elapsed();

            wait_for_rpc_monitor_block(&rpc_monitor, target_block, REPLICATION_TIMEOUT)
                .await
                .context("RPC monitor did not observe the restarted node's caught-up L2 head")?;

            anyhow::Ok((raft_caught_up_index, raft_caught_up_at, l2_caught_up_at))
        }
        .await;

        let rpc_report = rpc_monitor.stop().await;
        tracing::info!("restarted consensus node RPC monitor summary: {rpc_report}");
        tracing::info!(
            "restarted consensus node RPC monitor timeline:\n{}",
            rpc_report.format_detailed_timeline()
        );

        let all_node_indices = (0..cluster.len()).collect::<Vec<_>>();
        let final_l2_blocks = l2_block_snapshot(&cluster, &all_node_indices).await;
        let final_last_applied = raft_last_applied_snapshot(&cluster, &all_node_indices).await;

        let (raft_caught_up_index, raft_caught_up_at, l2_caught_up_at) =
            catch_up_result.with_context(|| {
                format!(
                    "restarted consensus node failed to catch up after long downtime: \
                     target_block={target_block}, target_applied={target_applied}, \
                     initial_block={restarted_node_initial_block}, active_nodes={active_node_indices:?}, \
                     l2_blocks=[{}], raft_last_applied=[{}], rpc_report={rpc_report}",
                    final_l2_blocks.join(", "),
                    final_last_applied.join(", "),
                )
            })?;

        rpc_report.assert_eventually_ready()?;
        let rpc_observed_target_at = first_rpc_observed_block_at(&rpc_report, target_block)
            .with_context(|| {
                format!(
                    "RPC monitor never observed restarted node reaching target block {target_block}: {rpc_report}"
                )
            })?;
        assert!(
            rpc_report
                .latest_block_number()
                .is_some_and(|block| block >= target_block),
            "RPC monitor latest block did not reach target {target_block}: {rpc_report}"
        );
        tracing::info!(
            raft_caught_up_index,
            raft_caught_up_ms = raft_caught_up_at.as_millis(),
            l2_caught_up_ms = l2_caught_up_at.as_millis(),
            rpc_observed_target_ms = rpc_observed_target_at.as_millis(),
            target_block,
            target_applied,
            "restarted consensus node caught up after long downtime"
        );

        let leader_idx = cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let post_rejoin_block =
            send_transfer_and_wait_for_l2_blocks(&cluster, leader_idx, &all_node_indices).await?;
        assert!(
            post_rejoin_block > target_block,
            "cluster did not keep producing after restarted node caught up: post_rejoin_block={post_rejoin_block}, target_block={target_block}"
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_while_transaction_storm_continues()
-> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::builder()
        .with_consensus_secret_keys(consensus_test_keys(3))
        .build()
        .await?;
    let result = async {
        cluster
            .wait_for_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;

        let restarted_node_idx = 2;
        let active_node_indices = (0..cluster.len())
            .filter(|idx| *idx != restarted_node_idx)
            .collect::<Vec<_>>();
        let all_node_indices = (0..cluster.len()).collect::<Vec<_>>();
        let restarted_node_rpc_url = cluster.node(restarted_node_idx).l2_rpc_url().to_owned();
        let restarted_node_initial_block = latest_l2_block(cluster.node(restarted_node_idx)).await?;

        cluster.suspend_node(restarted_node_idx).await?;
        let active_leader_idx = cluster
            .wait_for_active_raft_cluster_formation(CLUSTER_FORMATION_TIMEOUT)
            .await?;
        let active_follower_idx = active_node_indices
            .iter()
            .copied()
            .find(|idx| *idx != active_leader_idx)
            .expect("two active nodes should include one follower");

        let rpc_record_config = RpcRecordConfig {
            poll_interval: Duration::from_millis(200),
            request_timeout: Duration::from_secs(1),
            max_samples: 30_000,
        };
        let active_leader_rpc_monitor = HttpRpcRecorder::start_http(
            format!("active-leader-node-{active_leader_idx}"),
            cluster.node(active_leader_idx).l2_rpc_url().to_owned(),
            rpc_record_config.clone(),
        );
        let active_follower_rpc_monitor = HttpRpcRecorder::start_http(
            format!("active-follower-node-{active_follower_idx}"),
            cluster.node(active_follower_idx).l2_rpc_url().to_owned(),
            rpc_record_config.clone(),
        );
        let restarted_rpc_monitor = HttpRpcRecorder::start_http(
            format!("restarted-node-{restarted_node_idx}"),
            restarted_node_rpc_url,
            rpc_record_config,
        );

        let observation_result = async {
            let load_stats = generate_consensus_transaction_storm_across_restart(
                &mut cluster,
                &active_node_indices,
                restarted_node_idx,
                CONSENSUS_LONG_GAP_LOAD_DURATION,
                CONSENSUS_CONTINUED_LOAD_AFTER_RESTART_DURATION,
                &restarted_rpc_monitor,
            )
            .await?;
            let final_active_block = latest_l2_block(cluster.node(active_node_indices[0])).await?;
            assert!(
                load_stats.target_block_at_restart > restarted_node_initial_block,
                "active cluster head did not advance while node was down: initial={}, target_at_restart={}",
                restarted_node_initial_block,
                load_stats.target_block_at_restart
            );
            assert!(
                final_active_block > load_stats.target_block_at_restart,
                "active cluster did not keep producing after restart: final_active_block={}, target_at_restart={}",
                final_active_block,
                load_stats.target_block_at_restart
            );

            wait_for_l2_block(
                cluster.node(restarted_node_idx),
                final_active_block,
                CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT,
            )
            .await
            .context("restarted node did not catch up to the final active head after continued load")?;

            wait_for_rpc_monitor_block(
                &active_leader_rpc_monitor,
                final_active_block,
                REPLICATION_TIMEOUT,
            )
            .await
            .context("active leader RPC monitor did not observe the final active head")?;
            wait_for_rpc_monitor_block(
                &active_follower_rpc_monitor,
                final_active_block,
                REPLICATION_TIMEOUT,
            )
            .await
            .context("active follower RPC monitor did not observe the final active head")?;
            wait_for_rpc_monitor_block(
                &restarted_rpc_monitor,
                final_active_block,
                REPLICATION_TIMEOUT,
            )
            .await
            .context("restarted node RPC monitor did not observe the final active head")?;

            anyhow::Ok((load_stats, final_active_block))
        }
        .await;

        let active_leader_rpc_report = active_leader_rpc_monitor.stop().await;
        let active_follower_rpc_report = active_follower_rpc_monitor.stop().await;
        let restarted_rpc_report = restarted_rpc_monitor.stop().await;

        tracing::info!("active leader RPC monitor summary: {active_leader_rpc_report}");
        tracing::info!(
            "active leader RPC monitor timeline:\n{}",
            active_leader_rpc_report.format_detailed_timeline()
        );
        tracing::info!("active follower RPC monitor summary: {active_follower_rpc_report}");
        tracing::info!(
            "active follower RPC monitor timeline:\n{}",
            active_follower_rpc_report.format_detailed_timeline()
        );
        tracing::info!("restarted node RPC monitor summary: {restarted_rpc_report}");
        tracing::info!(
            "restarted node RPC monitor timeline:\n{}",
            restarted_rpc_report.format_detailed_timeline()
        );

        let final_l2_blocks = l2_block_snapshot(&cluster, &all_node_indices).await;
        let final_last_applied = raft_last_applied_snapshot(&cluster, &all_node_indices).await;
        let (load_stats, final_active_block) = observation_result.with_context(|| {
            format!(
                "restarted consensus node failed to catch up while load continued: \
                 initial_block={restarted_node_initial_block}, active_nodes={active_node_indices:?}, \
                 l2_blocks=[{}], raft_last_applied=[{}], \
                 active_leader_rpc_report={active_leader_rpc_report}, \
                 active_follower_rpc_report={active_follower_rpc_report}, \
                 restarted_rpc_report={restarted_rpc_report}",
                final_l2_blocks.join(", "),
                final_last_applied.join(", "),
            )
        })?;

        assert_rpc_monitor_stayed_ready(&active_leader_rpc_report)?;
        assert_rpc_monitor_stayed_ready(&active_follower_rpc_report)?;
        assert_rpc_monitor_recovered_after_outage(&restarted_rpc_report)?;
        let active_leader_final_at =
            first_rpc_observed_block_at(&active_leader_rpc_report, final_active_block)
                .context("active leader RPC monitor never observed final active block")?;
        let active_follower_final_at =
            first_rpc_observed_block_at(&active_follower_rpc_report, final_active_block)
                .context("active follower RPC monitor never observed final active block")?;
        let restarted_final_at =
            first_rpc_observed_block_at(&restarted_rpc_report, final_active_block)
                .context("restarted RPC monitor never observed final active block")?;

        tracing::info!(
            attempts = load_stats.attempts,
            tx_blocks = load_stats.blocks.len(),
            first_tx_block = load_stats.blocks.first().copied(),
            last_tx_block = load_stats.blocks.last().copied(),
            blocks_before_restart = load_stats.blocks_before_restart,
            blocks_after_restart = load_stats.blocks_after_restart,
            elapsed_ms = load_stats.elapsed.as_millis(),
            restart_started_ms = load_stats.restart_started_at.as_millis(),
            restart_completed_ms = load_stats.restart_completed_at.as_millis(),
            target_block_at_restart = load_stats.target_block_at_restart,
            target_applied_at_restart = load_stats.target_applied_at_restart,
            raft_caught_up_index = load_stats.raft_caught_up_index,
            raft_caught_up_ms = load_stats.raft_caught_up_at.as_millis(),
            l2_caught_up_ms = load_stats.l2_caught_up_at.as_millis(),
            rpc_caught_up_ms = load_stats.rpc_caught_up_at.as_millis(),
            final_active_block,
            active_leader_final_rpc_ms = active_leader_final_at.as_millis(),
            active_follower_final_rpc_ms = active_follower_final_at.as_millis(),
            restarted_final_rpc_ms = restarted_final_at.as_millis(),
            "restarted consensus node caught up while transaction storm continued"
        );

        Ok(())
    }
    .await;
    let shutdown_result = cluster.shutdown_all().await;
    result.and(shutdown_result)
}
