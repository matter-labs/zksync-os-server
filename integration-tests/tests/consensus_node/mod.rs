use alloy::eips::Encodable2718;
use alloy::network::{NetworkTransactionBuilder, ReceiptResponse, TransactionBuilder};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use anyhow::Context as _;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::multi_node::ConsensusCluster;

#[derive(Clone, Copy)]
pub(crate) enum TransferSubmission {
    SendTransaction,
    SendRawTransactionSync,
}

pub(crate) const CLUSTER_FORMATION_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const REPLICATION_TIMEOUT: Duration = Duration::from_secs(20);
const L1_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(60);
const TRANSFER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
const NO_QUORUM_PROGRESS_WINDOW: Duration = Duration::from_secs(5);
pub(crate) const CONSENSUS_PROGRESS_TIMEOUT: Duration = Duration::from_secs(90);

mod nonblocking;

/// Submit a single ETH transfer through the given live node and wait for the receipt.
///
/// Strict: one shot. If leadership shifts during the call, the receipt will reflect that
/// (transaction may revert) and the caller gets the error back. Tests that intentionally
/// exercise leadership churn must wrap this in a retry loop themselves.
pub(crate) async fn send_transfer(
    cluster: &ConsensusCluster,
    from_idx: usize,
) -> anyhow::Result<TransactionReceipt> {
    let node = cluster.node(from_idx);
    node.wait_for_initial_deposit()
        .await
        .with_context(|| format!("node {from_idx} did not become ready for L2 transfers"))?;

    tokio::time::timeout(TRANSFER_ATTEMPT_TIMEOUT, async {
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
    .with_context(|| format!("timed out sending transfer through node {from_idx}"))?
    .with_context(|| format!("failed sending transfer through node {from_idx}"))
}

/// Submit a transfer via either `eth_sendTransaction` or `eth_sendRawTransactionSync`. Returns
/// the included block number. Tests that don't care about the submission path should use
/// [`send_transfer`] instead.
pub(crate) async fn send_transfer_with(
    cluster: &ConsensusCluster,
    from_idx: usize,
    submission: TransferSubmission,
) -> anyhow::Result<u64> {
    let node = cluster.node(from_idx);
    node.wait_for_initial_deposit()
        .await
        .with_context(|| format!("node {from_idx} did not become ready for L2 transfers"))?;

    let recipient = Address::random();
    match submission {
        TransferSubmission::SendTransaction => {
            let gas_price = node.l2_provider.get_gas_price().await?;
            let tx = TransactionRequest::default()
                .with_to(recipient)
                .with_value(U256::from(1))
                .with_gas_price(gas_price);
            let receipt = node
                .l2_provider
                .send_transaction(tx)
                .await?
                .expect_successful_receipt()
                .await?;
            transfer_receipt_block_number(&receipt, recipient, None)
        }
        TransferSubmission::SendRawTransactionSync => {
            let sender = node.l2_wallet.default_signer().address();
            let fees = node.l2_provider.estimate_eip1559_fees().await?;
            let nonce = node.l2_provider.get_transaction_count(sender).await?;
            let tx = TransactionRequest::default()
                .with_to(recipient)
                .with_value(U256::from(1))
                .with_nonce(nonce)
                .with_gas_price(fees.max_fee_per_gas)
                .with_gas_limit(50_000);
            let tx_envelope = tx.build(&node.l2_wallet).await?;
            let expected_hash = *tx_envelope.tx_hash();
            let encoded = tx_envelope.encoded_2718();
            let receipt = node.l2_provider.send_raw_transaction_sync(&encoded).await?;
            transfer_receipt_block_number(&receipt, recipient, Some(expected_hash))
        }
    }
}

fn transfer_receipt_block_number(
    receipt: &impl ReceiptResponse,
    recipient: Address,
    expected_hash: Option<TxHash>,
) -> anyhow::Result<u64> {
    assert!(receipt.status());
    assert_eq!(receipt.to(), Some(recipient));
    let tx_hash = receipt.transaction_hash();
    if let Some(expected_hash) = expected_hash {
        assert_eq!(tx_hash, expected_hash);
    } else {
        assert_ne!(tx_hash, TxHash::ZERO);
    }
    receipt
        .block_number()
        .context("transfer receipt did not include a block number")
}

/// Convenience: send one transfer through `leader_idx` and wait for every live node to
/// expose the resulting block on its L2 RPC. Returns the block number.
///
/// Does NOT wait for L1 finalization — that is a separate subsystem and most consensus
/// assertions don't depend on it. Tests that need to assert finality should call
/// [`ConsensusCluster::wait_finalized_if_batcher_active`] explicitly afterwards.
pub(crate) async fn send_transfer_and_replicate(
    cluster: &ConsensusCluster,
    leader_idx: usize,
) -> anyhow::Result<u64> {
    let receipt = send_transfer(cluster, leader_idx).await?;
    let block_number = receipt
        .block_number
        .context("transfer receipt did not include a block number")?;
    cluster
        .wait_replicated(block_number, REPLICATION_TIMEOUT)
        .await?;
    Ok(block_number)
}

/// Send + replicate one block, retrying until either success or `timeout`. Use this when
/// the cluster is recovering from churn — quorum restoration, restarted node rejoin,
/// transaction storm — where a single shot is too narrow a window even though the cluster
/// will eventually produce. Each iteration re-runs `wait_healthy` so a freshly-restarted
/// or freshly-elected leader is picked up automatically.
pub(crate) async fn send_transfer_and_replicate_eventually(
    cluster: &mut ConsensusCluster,
    timeout: Duration,
) -> anyhow::Result<u64> {
    use tokio::time::{Instant, sleep};
    let deadline = Instant::now() + timeout;
    let mut attempts = 0;
    let mut last_error = None;
    while Instant::now() < deadline {
        let formation_timeout =
            CLUSTER_FORMATION_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
        let leader = match cluster.wait_healthy(formation_timeout).await {
            Ok(leader) => leader,
            Err(err) => {
                last_error = Some(format!("cluster not healthy: {err:#}"));
                sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        attempts += 1;
        match send_transfer_and_replicate(cluster, leader).await {
            Ok(block) => return Ok(block),
            Err(err) => {
                tracing::warn!(attempts, leader, error = %err, "send_transfer_and_replicate retry");
                last_error = Some(err.to_string());
                sleep(Duration::from_millis(200)).await;
            }
        }
    }
    anyhow::bail!(
        "timed out producing a consensus block: attempts={attempts}, last_error={last_error:?}"
    )
}

/// Asserts that the cluster cannot make progress for `NO_QUORUM_PROGRESS_WINDOW`: an attempted
/// transfer either fails or times out, and the L2 head observed by `survivor_idx` does not
/// advance.
pub(crate) async fn assert_no_progress_without_quorum(
    cluster: &ConsensusCluster,
    survivor_idx: usize,
) -> anyhow::Result<()> {
    let before = cluster.latest_l2_block(survivor_idx).await?;
    match tokio::time::timeout(
        NO_QUORUM_PROGRESS_WINDOW,
        send_transfer(cluster, survivor_idx),
    )
    .await
    {
        Ok(Ok(receipt)) => anyhow::bail!(
            "transaction unexpectedly produced a receipt without quorum (block={:?})",
            receipt.block_number
        ),
        Ok(Err(err)) => {
            tracing::info!(survivor_idx, error = %err, "transfer failed without quorum, as expected")
        }
        Err(_) => tracing::info!(survivor_idx, "transfer hung without quorum, as expected"),
    }
    let after = cluster.latest_l2_block(survivor_idx).await?;
    anyhow::ensure!(
        after == before,
        "L2 head advanced after quorum loss: before={before} after={after}"
    );
    Ok(())
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_includes_simple_transaction_with_wait() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(1).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let block = send_transfer_and_replicate(&cluster, leader).await?;
        cluster
            .wait_finalized_if_batcher_active(block, L1_FINALIZATION_TIMEOUT)
            .await?;
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_can_be_reenabled_after_clearing_raft_history() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(1).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        send_transfer(&cluster, leader).await?;
        send_transfer(&cluster, leader).await?;

        cluster.suspend(leader).await?;

        // Create a WAL/raft gap: clear raft history and produce a block through loopback
        // consensus while raft is disabled.
        cluster
            .restart_with_overrides(leader, |cfg| {
                cfg.consensus_config.enabled = false;
                cfg.consensus_config.force_clear_raft_history = true;
            })
            .await?;
        send_transfer(&cluster, leader).await?;

        cluster.suspend(leader).await?;

        // Re-enable consensus after the gap. The old WAL blocks replay locally; new blocks
        // are raft-canonized from this point.
        cluster
            .restart_with_overrides(leader, |cfg| {
                cfg.consensus_config.enabled = true;
                cfg.consensus_config.force_clear_raft_history = false;
            })
            .await?;
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        send_transfer(&cluster, leader).await?;

        // Restart once more with consensus enabled to verify the sparse raft history
        // written after re-enable is loadable.
        cluster.suspend(leader).await?;
        cluster.start(leader).await?;
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        send_transfer(&cluster, leader).await?;

        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_forms_with_three_nodes_and_replicates_blocks() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        send_transfer_and_replicate(&cluster, leader).await?;
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}

#[test_log::test(tokio::test)]
async fn consensus_cluster_recovers_after_quorum_loss() -> anyhow::Result<()> {
    let mut cluster = ConsensusCluster::builder().nodes(3).build().await?;
    let result = async {
        let leader = cluster.wait_healthy(CLUSTER_FORMATION_TIMEOUT).await?;
        let committed_block = send_transfer_and_replicate(&cluster, leader).await?;

        let followers: Vec<_> = cluster
            .live_indices()
            .into_iter()
            .filter(|idx| *idx != leader)
            .collect();
        cluster.suspend(followers[0]).await?;
        cluster.suspend(followers[1]).await?;

        // Quorum lost: no progress can be made.
        assert_no_progress_without_quorum(&cluster, leader).await?;

        // Restore quorum and verify the cluster recovers and makes progress. Use the
        // retrying helper because the leader's produce pipeline may need a moment to
        // unstick after the quorum-loss window; a one-shot send is too narrow.
        cluster.start(followers[0]).await?;
        let recovery_block =
            send_transfer_and_replicate_eventually(&mut cluster, CONSENSUS_PROGRESS_TIMEOUT)
                .await?;
        assert!(
            recovery_block > committed_block,
            "cluster must make progress after quorum is restored: committed={committed_block} recovery={recovery_block}",
        );
        Ok(())
    }
    .await;
    result.and(cluster.shutdown_all().await)
}
