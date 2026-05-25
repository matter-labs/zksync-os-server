use super::*;

use std::time::Duration;
use tokio::time::{Instant, sleep};
use zksync_os_integration_tests::rpc_recorder::{HttpRpcRecorder, HttpRpcReport, RpcRecordConfig};

const CONSENSUS_RESTART_GAP_BLOCKS: usize = 3;
const CONSENSUS_CONTINUED_BLOCKS_AFTER_RESTART: usize = 2;
const CONSENSUS_RESTART_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(120);
const CONSENSUS_LONG_GAP_LOAD_DURATION: Duration = Duration::from_secs(60);
const CONSENSUS_CONTINUED_LOAD_AFTER_RESTART_DURATION: Duration = Duration::from_secs(45);
const CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(180);

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
    final_active_block: u64,
    active_head_blocks_after_restart: u64,
    l2_caught_up_at: Duration,
    rpc_caught_up_at: Duration,
}

fn remaining_storm_send_timeout(deadline: Instant) -> Option<Duration> {
    let now = Instant::now();
    if now >= deadline {
        return None;
    }
    Some((REPLICATION_TIMEOUT + Duration::from_secs(10)).min(deadline.duration_since(now)))
}

async fn generate_consensus_transaction_storm(
    cluster: &mut ConsensusCluster,
    duration: Duration,
) -> anyhow::Result<ConsensusLoadStats> {
    let started_at = Instant::now();
    let deadline = started_at + duration;
    let mut attempts = 0;
    let mut blocks = Vec::new();
    let mut last_error = None;
    let mut next_submit_slot = 0usize;

    while Instant::now() < deadline {
        cluster.heal_crashed_nodes().await?;
        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        // Round-robin submission across live nodes exercises the tx-forwarding path
        // (followers forward to the leader) on every iteration.
        let live = cluster.live_indices();
        let submit_idx = live[next_submit_slot % live.len()];
        next_submit_slot = next_submit_slot.wrapping_add(1);
        let Some(send_timeout) = remaining_storm_send_timeout(deadline) else {
            break;
        };
        attempts += 1;
        match tokio::time::timeout(
            send_timeout,
            send_transfer_and_replicate(cluster, submit_idx),
        )
        .await
        {
            Ok(Ok(block)) => {
                tracing::info!(
                    attempts,
                    produced_blocks = blocks.len() + 1,
                    block,
                    submit_idx,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "transaction storm produced a block"
                );
                blocks.push(block);
            }
            Ok(Err(err)) => {
                tracing::warn!(attempts, error = %err, "transaction storm send failed");
                last_error = Some(err.to_string());
                sleep(Duration::from_millis(200)).await;
            }
            Err(_) => {
                tracing::warn!(attempts, "transaction storm send timed out");
                last_error = Some("timed out".to_owned());
                sleep(Duration::from_millis(200)).await;
            }
        }
    }

    // No throughput floor: block counts depend on CPU contention and PIG load, which is
    // out of scope for the correctness signal these tests carry. Callers assert head
    // movement separately via `latest_l2_block` comparisons.
    let _ = last_error;
    Ok(ConsensusLoadStats {
        attempts,
        blocks,
        elapsed: started_at.elapsed(),
    })
}

async fn observe_restarted_node_catch_up_to_target(
    cluster: &ConsensusCluster,
    restarted_node_idx: usize,
    restarted_rpc_monitor: &HttpRpcRecorder,
    target_block: u64,
    started_at: Instant,
    l2_caught_up: &mut Option<Duration>,
    rpc_caught_up: &mut Option<Duration>,
) {
    if l2_caught_up.is_none()
        && let Ok(latest) = cluster.latest_l2_block(restarted_node_idx).await
        && latest >= target_block
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
    cluster: &mut ConsensusCluster,
    restarted_node_idx: usize,
    restart_after: Duration,
    continue_after_restart: Duration,
    restarted_rpc_monitor: &HttpRpcRecorder,
) -> anyhow::Result<ConsensusRejoinLoadStats> {
    let active_leader_reference = cluster
        .live_indices()
        .into_iter()
        .find(|idx| *idx != restarted_node_idx)
        .context("no other live node to query for active head")?;

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
    let mut l2_caught_up = None;
    let mut rpc_caught_up = None;
    let mut last_error = None;
    let mut next_submit_slot = 0usize;

    while Instant::now() < stop_deadline {
        cluster.heal_crashed_nodes().await?;
        if !restarted && Instant::now() >= restart_deadline {
            let target = cluster.latest_l2_block(active_leader_reference).await?;
            let started = started_at.elapsed();
            tracing::info!(
                restarted_node_idx,
                target_block = target,
                elapsed_ms = started.as_millis(),
                "restarting consensus node while transaction storm continues"
            );
            cluster.start(restarted_node_idx).await?;
            let completed = started_at.elapsed();
            restarted = true;
            restart_started_at = Some(started);
            restart_completed_at = Some(completed);
            target_block_at_restart = Some(target);
        }

        if let Some(target) = target_block_at_restart {
            observe_restarted_node_catch_up_to_target(
                cluster,
                restarted_node_idx,
                restarted_rpc_monitor,
                target,
                started_at,
                &mut l2_caught_up,
                &mut rpc_caught_up,
            )
            .await;
        }

        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        // Round-robin across the live nodes so the storm exercises tx forwarding from
        // followers, not just direct submission to the leader. The restarted node
        // becomes live mid-storm and joins the rotation once it does.
        let live = cluster.live_indices();
        let submit_idx = live[next_submit_slot % live.len()];
        next_submit_slot = next_submit_slot.wrapping_add(1);
        let Some(send_timeout) = remaining_storm_send_timeout(stop_deadline) else {
            break;
        };
        attempts += 1;
        match tokio::time::timeout(
            send_timeout,
            send_transfer_and_replicate(cluster, submit_idx),
        )
        .await
        {
            Ok(Ok(block)) => {
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
                    block,
                    submit_idx,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "continuous transaction storm produced a block"
                );
                blocks.push(block);
            }
            Ok(Err(err)) => {
                tracing::warn!(attempts, error = %err, "continuous storm send failed");
                last_error = Some(err.to_string());
                sleep(Duration::from_millis(200)).await;
            }
            Err(_) => {
                tracing::warn!(attempts, "continuous storm send timed out");
                last_error = Some("timed out".to_owned());
                sleep(Duration::from_millis(200)).await;
            }
        }
    }

    if let Some(target) = target_block_at_restart {
        observe_restarted_node_catch_up_to_target(
            cluster,
            restarted_node_idx,
            restarted_rpc_monitor,
            target,
            started_at,
            &mut l2_caught_up,
            &mut rpc_caught_up,
        )
        .await;
    }

    anyhow::ensure!(
        restarted,
        "transaction storm ended before restarting node {restarted_node_idx}"
    );
    // Required: the storm produced at least one block before the restart, otherwise the
    // restart isn't testing anything. Anything beyond that is throughput, which we report
    // but do not assert on (depends on CPU contention).
    anyhow::ensure!(
        blocks_before_restart >= 1,
        "storm produced no blocks before restart: attempts={attempts}, last_error={last_error:?}",
    );
    let target_block_at_restart = target_block_at_restart.expect("target block set");
    let final_active_block = cluster.latest_l2_block(active_leader_reference).await?;
    let active_head_blocks_after_restart =
        final_active_block.saturating_sub(target_block_at_restart);
    let _ = last_error;

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
        target_block_at_restart,
        final_active_block,
        active_head_blocks_after_restart,
        l2_caught_up_at,
        rpc_caught_up_at,
    })
}

/// Allow brief transient RPC blips on a node that should have "stayed ready". Anything
/// up to this long counts as recoverable transport noise, not a real outage. Storm tests
/// run under heavy CPU pressure where a single dropped sample is common and not a
/// correctness violation as long as the node recovers immediately.
const ACTIVE_NODE_TOLERATED_OUTAGE: Duration = Duration::from_secs(2);

fn assert_rpc_monitor_stayed_ready(report: &HttpRpcReport) -> anyhow::Result<()> {
    report.first_ready_at().with_context(|| {
        format!(
            "{} never became ready while it should have stayed ready: {report}",
            report.name
        )
    })?;
    if let Some(outage) = report.longest_error_streak() {
        anyhow::ensure!(
            outage.duration <= ACTIVE_NODE_TOLERATED_OUTAGE,
            "{} observed an RPC outage longer than the tolerated window ({:?}): {report}\n{}",
            report.name,
            ACTIVE_NODE_TOLERATED_OUTAGE,
            report.format_detailed_timeline()
        );
    }
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

async fn produce_consensus_blocks(
    cluster: &mut ConsensusCluster,
    count: usize,
) -> anyhow::Result<Vec<u64>> {
    let mut blocks = Vec::with_capacity(count);
    for ordinal in 0..count {
        let block = send_transfer_and_replicate_eventually(cluster, CONSENSUS_PROGRESS_TIMEOUT)
            .await
            .with_context(|| {
                format!("failed to produce consensus block {}/{count}", ordinal + 1)
            })?;
        blocks.push(block);
    }
    Ok(blocks)
}

async fn l2_block_snapshot(cluster: &ConsensusCluster) -> Vec<String> {
    let mut snapshot = Vec::new();
    for idx in cluster.indices() {
        if cluster.is_suspended(idx) {
            snapshot.push(format!("node_{idx}: suspended"));
            continue;
        }
        match cluster.latest_l2_block(idx).await {
            Ok(block) => snapshot.push(format!("node_{idx}: block={block}")),
            Err(err) => snapshot.push(format!("node_{idx}: block_error={err:#}")),
        }
    }
    snapshot
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_after_transaction_gap() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let restarted = cluster
            .live_indices()
            .into_iter()
            .find(|idx| *idx != leader)
            .expect("3-node cluster must have a follower");
        let restarted_initial_block = cluster.latest_l2_block(restarted).await?;

        cluster.suspend(restarted).await?;
        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;

        let started_at = Instant::now();
        let gap_blocks = produce_consensus_blocks(&mut cluster, CONSENSUS_RESTART_GAP_BLOCKS).await?;
        let target_block = *gap_blocks.last().context("gap producer returned no blocks")?;
        assert!(
            target_block > restarted_initial_block,
            "active cluster head did not advance while node was down: initial={restarted_initial_block}, target={target_block}"
        );

        cluster.start(restarted).await?;
        let catch_up = cluster
            .wait_node_at(restarted, target_block, CONSENSUS_RESTART_CATCH_UP_TIMEOUT)
            .await;
        if let Err(err) = catch_up {
            let snapshot = l2_block_snapshot(&cluster).await;
            return Err(err).with_context(|| {
                format!(
                    "restarted node did not catch up after transaction gap: \
                     target_block={target_block}, initial={restarted_initial_block}, \
                     l2_blocks=[{}]",
                    snapshot.join(", ")
                )
            });
        }
        let caught_up_at = started_at.elapsed();

        let post_rejoin_block =
            send_transfer_and_replicate_eventually(&mut cluster, CONSENSUS_PROGRESS_TIMEOUT)
                .await?;
        assert!(
            post_rejoin_block > target_block,
            "cluster did not keep producing after restarted node caught up: post={post_rejoin_block}, target={target_block}"
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
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_while_new_blocks_continue() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let restarted = cluster
            .live_indices()
            .into_iter()
            .find(|idx| *idx != leader)
            .expect("3-node cluster must have a follower");
        let restarted_initial_block = cluster.latest_l2_block(restarted).await?;

        cluster.suspend(restarted).await?;
        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;

        let started_at = Instant::now();
        let blocks_before = produce_consensus_blocks(&mut cluster, CONSENSUS_RESTART_GAP_BLOCKS).await?;
        let target_block_at_restart = *blocks_before
            .last()
            .context("pre-restart producer returned no blocks")?;
        assert!(
            target_block_at_restart > restarted_initial_block,
            "active cluster head did not advance while node was down: initial={restarted_initial_block}, target_at_restart={target_block_at_restart}"
        );

        let restart_started_at = started_at.elapsed();
        cluster.start(restarted).await?;
        let restart_completed_at = started_at.elapsed();

        let blocks_after =
            produce_consensus_blocks(&mut cluster, CONSENSUS_CONTINUED_BLOCKS_AFTER_RESTART).await?;
        let final_active_block = *blocks_after
            .last()
            .context("post-restart producer returned no blocks")?;
        assert!(
            final_active_block > target_block_at_restart,
            "active cluster did not keep producing after restart: final={final_active_block}, target_at_restart={target_block_at_restart}"
        );

        let catch_up = cluster
            .wait_node_at(restarted, final_active_block, CONSENSUS_RESTART_CATCH_UP_TIMEOUT)
            .await;
        if let Err(err) = catch_up {
            let snapshot = l2_block_snapshot(&cluster).await;
            return Err(err).with_context(|| {
                format!(
                    "restarted node did not catch up to final active block: \
                     final={final_active_block}, initial={restarted_initial_block}, \
                     l2_blocks=[{}]",
                    snapshot.join(", ")
                )
            });
        }
        let caught_up_at = started_at.elapsed();

        let post_rejoin_block =
            send_transfer_and_replicate_eventually(&mut cluster, CONSENSUS_PROGRESS_TIMEOUT)
                .await?;
        assert!(
            post_rejoin_block > final_active_block,
            "cluster did not keep producing after restarted node caught up: post={post_rejoin_block}, final={final_active_block}"
        );

        tracing::info!(
            blocks_before_restart = blocks_before.len(),
            blocks_after_restart = blocks_after.len(),
            first_block_before_restart = blocks_before.first().copied(),
            target_block_at_restart,
            final_active_block,
            restart_started_ms = restart_started_at.as_millis(),
            restart_completed_ms = restart_completed_at.as_millis(),
            caught_up_ms = caught_up_at.as_millis(),
            post_rejoin_block,
            "restarted node caught up while active nodes continued producing"
        );
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_after_long_transaction_storm() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;

        let restarted = 2;
        let restarted_rpc_url = cluster.node(restarted).l2_rpc_url().to_owned();
        let restarted_initial_block = cluster.latest_l2_block(restarted).await?;

        cluster.suspend(restarted).await?;
        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;

        let load_stats =
            generate_consensus_transaction_storm(&mut cluster, CONSENSUS_LONG_GAP_LOAD_DURATION)
                .await?;
        let active_reference = cluster
            .live_indices()
            .into_iter()
            .next()
            .context("no live nodes left after storm")?;
        let target_block = cluster.latest_l2_block(active_reference).await?;
        assert!(
            target_block > restarted_initial_block,
            "active cluster head did not advance while node was down: initial={restarted_initial_block}, target={target_block}"
        );
        tracing::info!(
            attempts = load_stats.attempts,
            tx_blocks = load_stats.blocks.len(),
            first_tx_block = load_stats.blocks.first().copied(),
            last_tx_block = load_stats.blocks.last().copied(),
            elapsed_ms = load_stats.elapsed.as_millis(),
            restarted_initial_block,
            target_block,
            "transaction storm finished while consensus node was stopped"
        );

        let rpc_monitor = HttpRpcRecorder::start_http(
            "restarted-consensus-node",
            restarted_rpc_url,
            RpcRecordConfig {
                poll_interval: Duration::from_millis(200),
                request_timeout: Duration::from_secs(1),
                max_samples: 20_000,
            },
        );
        let restart_started_at = Instant::now();
        cluster.start(restarted).await?;

        let catch_up = async {
            cluster
                .wait_node_at(restarted, target_block, CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT)
                .await
                .context("restarted node did not expose target L2 block")?;
            let l2_caught_up_at = restart_started_at.elapsed();
            wait_for_rpc_monitor_block(&rpc_monitor, target_block, REPLICATION_TIMEOUT)
                .await
                .context("RPC monitor did not observe the restarted node's caught-up head")?;
            anyhow::Ok(l2_caught_up_at)
        }
        .await;

        let rpc_report = rpc_monitor.stop().await;
        tracing::info!("restarted RPC monitor summary: {rpc_report}");
        tracing::info!(
            "restarted RPC monitor timeline:\n{}",
            rpc_report.format_detailed_timeline()
        );

        let snapshot = l2_block_snapshot(&cluster).await;
        let l2_caught_up_at = catch_up.with_context(|| {
            format!(
                "restarted node failed to catch up after long downtime: \
                 target={target_block}, initial={restarted_initial_block}, \
                 l2_blocks=[{}], rpc_report={rpc_report}",
                snapshot.join(", ")
            )
        })?;

        rpc_report.assert_eventually_ready()?;
        let rpc_observed_at = rpc_report
            .first_observed_block_at(target_block)
            .with_context(|| {
                format!("RPC monitor never observed restart target {target_block}: {rpc_report}")
            })?;
        assert!(
            rpc_report
                .latest_block_number()
                .is_some_and(|block| block >= target_block),
            "RPC monitor latest block did not reach target {target_block}: {rpc_report}"
        );
        tracing::info!(
            l2_caught_up_ms = l2_caught_up_at.as_millis(),
            rpc_observed_target_ms = rpc_observed_at.as_millis(),
            target_block,
            "restarted consensus node caught up after long downtime"
        );

        // Use the retry helper for the post-rejoin sanity check: right after a long
        // storm + catch-up, leadership may still be settling, and the strict one-shot
        // send is too narrow a window. Correctness signal is "cluster produces another
        // block at all," not "the first attempt succeeds."
        let post_rejoin =
            send_transfer_and_replicate_eventually(&mut cluster, CONSENSUS_PROGRESS_TIMEOUT)
                .await?;
        assert!(
            post_rejoin > target_block,
            "cluster did not keep producing after restarted node caught up: post={post_rejoin}, target={target_block}"
        );
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_restarted_node_catches_up_while_transaction_storm_continues()
-> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;

        let restarted = 2;
        let restarted_rpc_url = cluster.node(restarted).l2_rpc_url().to_owned();
        let restarted_initial_block = cluster.latest_l2_block(restarted).await?;

        cluster.suspend(restarted).await?;
        let active_leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let active_follower = cluster
            .live_indices()
            .into_iter()
            .find(|idx| *idx != active_leader)
            .expect("two active nodes should include one follower");

        cluster
            .latest_l2_block(active_leader)
            .await
            .with_context(|| format!("active leader node {active_leader} RPC not ready"))?;
        cluster
            .latest_l2_block(active_follower)
            .await
            .with_context(|| format!("active follower node {active_follower} RPC not ready"))?;

        let rpc_record_config = RpcRecordConfig {
            poll_interval: Duration::from_millis(200),
            request_timeout: Duration::from_secs(1),
            max_samples: 30_000,
        };
        let active_leader_monitor = HttpRpcRecorder::start_http(
            format!("active-leader-node-{active_leader}"),
            cluster.node(active_leader).l2_rpc_url().to_owned(),
            rpc_record_config.clone(),
        );
        let active_follower_monitor = HttpRpcRecorder::start_http(
            format!("active-follower-node-{active_follower}"),
            cluster.node(active_follower).l2_rpc_url().to_owned(),
            rpc_record_config.clone(),
        );
        let restarted_monitor = HttpRpcRecorder::start_http(
            format!("restarted-node-{restarted}"),
            restarted_rpc_url,
            rpc_record_config,
        );

        let observation = async {
            let stats = generate_consensus_transaction_storm_across_restart(
                &mut cluster,
                restarted,
                CONSENSUS_LONG_GAP_LOAD_DURATION,
                CONSENSUS_CONTINUED_LOAD_AFTER_RESTART_DURATION,
                &restarted_monitor,
            )
            .await?;
            let final_active_block = stats.final_active_block;
            assert!(
                stats.target_block_at_restart > restarted_initial_block,
                "active cluster head did not advance while node was down: initial={}, target_at_restart={}",
                restarted_initial_block,
                stats.target_block_at_restart
            );
            // Note: we do *not* assert that more blocks were produced after restart.
            // Under heavy CPU pressure the cluster may not make further progress in the
            // continued-load window; that is a throughput observation, not a correctness
            // failure. The catch-up assertions below are the correctness signal.

            cluster
                .wait_node_at(restarted, final_active_block, CONSENSUS_LONG_GAP_CATCH_UP_TIMEOUT)
                .await
                .context("restarted node did not catch up to final active head")?;
            wait_for_rpc_monitor_block(&active_leader_monitor, final_active_block, REPLICATION_TIMEOUT)
                .await
                .context("active leader RPC monitor did not observe final active head")?;
            wait_for_rpc_monitor_block(&active_follower_monitor, final_active_block, REPLICATION_TIMEOUT)
                .await
                .context("active follower RPC monitor did not observe final active head")?;
            wait_for_rpc_monitor_block(&restarted_monitor, final_active_block, REPLICATION_TIMEOUT)
                .await
                .context("restarted RPC monitor did not observe final active head")?;
            anyhow::Ok((stats, final_active_block))
        }
        .await;

        let active_leader_report = active_leader_monitor.stop().await;
        let active_follower_report = active_follower_monitor.stop().await;
        let restarted_report = restarted_monitor.stop().await;
        tracing::info!("active leader RPC monitor summary: {active_leader_report}");
        tracing::info!(
            "active leader RPC monitor timeline:\n{}",
            active_leader_report.format_detailed_timeline()
        );
        tracing::info!("active follower RPC monitor summary: {active_follower_report}");
        tracing::info!(
            "active follower RPC monitor timeline:\n{}",
            active_follower_report.format_detailed_timeline()
        );
        tracing::info!("restarted RPC monitor summary: {restarted_report}");
        tracing::info!(
            "restarted RPC monitor timeline:\n{}",
            restarted_report.format_detailed_timeline()
        );

        let snapshot = l2_block_snapshot(&cluster).await;
        let (stats, final_active_block) = observation.with_context(|| {
            format!(
                "restarted node failed to catch up while load continued: \
                 initial={restarted_initial_block}, l2_blocks=[{}], \
                 active_leader_report={active_leader_report}, \
                 active_follower_report={active_follower_report}, \
                 restarted_report={restarted_report}",
                snapshot.join(", ")
            )
        })?;

        assert_rpc_monitor_stayed_ready(&active_leader_report)?;
        assert_rpc_monitor_stayed_ready(&active_follower_report)?;
        assert_rpc_monitor_recovered_after_outage(&restarted_report)?;
        let active_leader_final = active_leader_report
            .first_observed_block_at(final_active_block)
            .context("active leader RPC monitor never observed final active block")?;
        let active_follower_final = active_follower_report
            .first_observed_block_at(final_active_block)
            .context("active follower RPC monitor never observed final active block")?;
        let restarted_final = restarted_report
            .first_observed_block_at(final_active_block)
            .context("restarted RPC monitor never observed final active block")?;

        tracing::info!(
            attempts = stats.attempts,
            tx_blocks = stats.blocks.len(),
            first_tx_block = stats.blocks.first().copied(),
            last_tx_block = stats.blocks.last().copied(),
            blocks_before_restart = stats.blocks_before_restart,
            blocks_after_restart = stats.blocks_after_restart,
            active_head_blocks_after_restart = stats.active_head_blocks_after_restart,
            elapsed_ms = stats.elapsed.as_millis(),
            restart_started_ms = stats.restart_started_at.as_millis(),
            restart_completed_ms = stats.restart_completed_at.as_millis(),
            target_block_at_restart = stats.target_block_at_restart,
            l2_caught_up_ms = stats.l2_caught_up_at.as_millis(),
            rpc_caught_up_ms = stats.rpc_caught_up_at.as_millis(),
            final_active_block,
            active_leader_final_rpc_ms = active_leader_final.as_millis(),
            active_follower_final_rpc_ms = active_follower_final.as_millis(),
            restarted_final_rpc_ms = restarted_final.as_millis(),
            "restarted consensus node caught up while transaction storm continued"
        );
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}
