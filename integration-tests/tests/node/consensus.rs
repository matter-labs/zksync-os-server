//! BFT consensus over real nodes: committees of full in-process nodes over one L1,
//! producing, verifying, and finalizing real blocks — including validators stopping
//! and rejoining.
//!
//! Reaching the first assertion of any test here already proves a lot: node startup
//! waits for the initial L1 deposit to be *included in a block*, which under consensus
//! requires the committee to form over p2p, a leader to build a block carrying the L1
//! priority transaction, the other validators to re-execute and vote for it, and the
//! finalized block to flow through every node's persistence pipeline.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::l1_helpers::wait_for_l1_state;
use zksync_os_integration_tests::multi_node::MultiNodeTester;

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(60);

/// Submits a transfer through the validator at `via` and waits for its inclusion.
/// Returns the block number the transaction landed in.
async fn send_transfer(
    cluster: &MultiNodeTester,
    via: usize,
    recipient: Address,
) -> anyhow::Result<u64> {
    let receipt = tokio::time::timeout(CONVERGENCE_TIMEOUT, async {
        cluster
            .node(via)
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(recipient)
                    .with_value(U256::from(1_000_000u64)),
            )
            .await?
            .expect_successful_receipt()
            .await
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("transaction was not included within {CONVERGENCE_TIMEOUT:?}")
    })??;
    Ok(receipt
        .block_number
        .expect("included transactions have a block"))
}

#[test_log::test(tokio::test)]
async fn three_validators_finalize_and_agree() -> anyhow::Result<()> {
    let cluster = MultiNodeTester::start(3).await?;

    // A user transaction, submitted to a NON-batcher validator: it sits in that
    // validator's mempool until that validator's turn as leader, then rides a block
    // like any other transaction.
    let recipient = Address::repeat_byte(0x42);
    let value = U256::from(1_000_000u64);
    let receipt = tokio::time::timeout(CONVERGENCE_TIMEOUT, async {
        cluster
            .node(1)
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(recipient)
                    .with_value(value),
            )
            .await?
            .expect_successful_receipt()
            .await
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("transaction was not included within {CONVERGENCE_TIMEOUT:?}")
    })??;
    let included_at = receipt
        .block_number
        .expect("included transactions have a block");

    // Every validator converges on the same chain: same height reached, identical
    // block hash where the transaction landed, and the state effect visible everywhere.
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(included_at).await?;
    for index in 0..cluster.len() {
        let balance = cluster
            .node(index)
            .l2_provider
            .get_balance(recipient)
            .await?;
        assert_eq!(balance, value, "validator {index} sees a different balance");
    }

    // Liveness: the chain keeps growing past the transaction.
    cluster
        .wait_for_block_on_all(included_at + 5, CONVERGENCE_TIMEOUT)
        .await?;

    // The observability surfaces external monitors poll: `/status` reports consensus
    // progress, and the consensus runtime's own registry is served for scraping.
    let status = cluster.node(1).status().await?;
    let consensus = status
        .consensus
        .expect("validators must report a consensus status section");
    assert_eq!(consensus.committee_size, 3);
    let finalized = consensus
        .finalized
        .expect("a finalized round must have been observed by now");
    assert!(finalized.view > 0, "finalized view must have advanced");
    assert!(
        consensus.applied_height.unwrap_or(0) >= included_at,
        "applied height must cover the included transaction",
    );
    let metrics = cluster.node(1).consensus_metrics().await?;
    assert!(
        !metrics.is_empty(),
        "the consensus runtime's metrics registry must serve content",
    );

    cluster.shutdown_all().await
}

/// A validator that restarts on its own state must rejoin the committee, backfill the
/// blocks it missed, and *participate* again — the full restart surface in one flow:
/// consensus journal replay, the committed-tip digest re-anchor, cold-path commits
/// (re-execution of backfilled blocks), the mempool re-init, and the L1 watcher
/// re-scan that verification's input authenticity depends on.
#[test_log::test(tokio::test)]
async fn validator_restart_rejoins_catches_up_and_votes_again() -> anyhow::Result<()> {
    // Four validators: a committee that tolerates one fault, so the chain keeps
    // running while one validator is down.
    let mut cluster = MultiNodeTester::start(4).await?;

    let included_at = send_transfer(&cluster, 1, Address::repeat_byte(0x51)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;

    // Take a follower down; the remaining three are exactly quorum and keep
    // finalizing (this validator's leader turns simply time out).
    cluster.stop_validator(3).await?;
    let while_down = send_transfer(&cluster, 1, Address::repeat_byte(0x52)).await?;
    cluster
        .wait_for_block_on_all(while_down + 3, CONVERGENCE_TIMEOUT)
        .await?;

    // Restart it on its original state and keys. It must catch up to the live tip —
    // which keeps moving — and serve the blocks it missed identically.
    cluster.start_validator(3).await?;
    let target = cluster.max_height().await? + 3;
    cluster
        .wait_for_block_on_all(target, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(while_down).await?;

    // Catching up proves it *follows*; now prove it *participates*. With a different
    // validator stopped, the remaining three are exactly quorum again — every new
    // finalization now requires the restarted validator's vote, and its vote requires
    // it to verify fresh proposals (validity rules + re-execution) on post-restart
    // state.
    cluster.stop_validator(1).await?;
    let after_rejoin = send_transfer(&cluster, 2, Address::repeat_byte(0x53)).await?;
    cluster
        .wait_for_block_on_all(after_rejoin, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(after_rejoin).await?;

    cluster.shutdown_all().await
}

/// The whole committee must survive an L1 outage — degrade, never die. Every
/// validator loses its L1 RPC at once (the shared-provider-outage shape, and the
/// chaos rig's partition fault), for longer than the provider-level retry budget
/// that used to be the only buffer before background components crashed the node.
/// While L1 is away the chain must keep finalizing L2 traffic; when L1 returns,
/// settlement must resume on its own. Found by the chaos rig's third soak (three
/// distinct death mechanisms: the upgrade gatekeeper, the L1 senders, and the
/// provider's own header pollers).
///
/// Four validators, deliberately: the batcher validator's pipeline intentionally
/// pauses on its bounded runway once settlement backs up (the speculative-state
/// cap), which makes that one node withhold votes until L1 returns — the committee
/// must ride through that like any single-node fault, which needs n >= 4.
#[test_log::test(tokio::test)]
async fn committee_survives_l1_outage() -> anyhow::Result<()> {
    let (cluster, mut l1_proxy) = MultiNodeTester::start_with_severable_l1(4).await?;

    // Healthy baseline: real traffic and at least one batch committed on L1, so the
    // L1 senders are mid-flight when the outage begins.
    let included_at = send_transfer(&cluster, 1, Address::repeat_byte(0x81)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    let before_outage = wait_for_l1_state(cluster.node(0), "a batch committed on L1", |state| {
        state.last_committed_batch >= 1
    })
    .await?;

    // The outage: refuse every L1 connection for well past the provider's internal
    // retry budget (~40s was enough to kill a node before the fix).
    l1_proxy.sever().await;
    tokio::time::sleep(Duration::from_secs(60)).await;

    // Mid-outage, the committee must still be finalizing: an L2 transfer needs no
    // L1 input and must land normally.
    let during_outage = send_transfer(&cluster, 1, Address::repeat_byte(0x82)).await?;
    cluster
        .wait_for_block_on_all(during_outage, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(during_outage).await?;
    for index in 0..cluster.len() {
        let status = cluster.node(index).status().await?;
        anyhow::ensure!(
            status.healthy,
            "validator {index} reports unhealthy during the L1 outage"
        );
    }

    // L1 returns: settlement must resume without any restart.
    l1_proxy.restore().await?;
    wait_for_l1_state(
        cluster.node(0),
        "batch settlement resumes after the outage",
        |state| state.last_committed_batch > before_outage.last_committed_batch,
    )
    .await?;

    cluster.shutdown_all().await
}

/// The batcher validator must survive a restart. Its recovery is the strictest in the
/// committee: the batcher resumes from the last L1-*executed* batch and re-creates
/// every batch already committed on L1, which requires the pipeline to replay blocks
/// from that point — strictly below the WAL tip where the live consensus stream
/// resumes. Found by the chaos rig's first soak: consensus mode fed the pipeline only
/// live finalized blocks, and the restarted batcher died on the gap ("Existing batch
/// first block (N) does not match next block in stream (M)").
#[test_log::test(tokio::test)]
async fn batcher_validator_restart_recreates_batches_and_keeps_settling() -> anyhow::Result<()> {
    // Four validators: the chain keeps finalizing while the batcher validator is
    // down, so it restarts into a moving committee, replays its own write-ahead log,
    // and backfills the rest — the exact shape of the chaos-rig failure.
    let mut cluster = MultiNodeTester::start(4).await?;

    // Real batches on L1: wait until a batch is committed whose execution has not
    // landed yet. That is precisely the state whose recovery was broken — and it
    // stays that way while the batcher validator is down, because both the commit
    // and the execute senders live on that node.
    let included_at = send_transfer(&cluster, 1, Address::repeat_byte(0x61)).await?;
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    let before_restart = wait_for_l1_state(
        cluster.node(0),
        "a batch committed but not yet executed on L1",
        |state| state.last_committed_batch > state.last_executed_batch,
    )
    .await?;

    // Restart the batcher validator while that recovery window is open; the chain
    // moves on without it in the meantime.
    cluster.stop_validator(0).await?;
    let while_down = send_transfer(&cluster, 1, Address::repeat_byte(0x62)).await?;
    cluster
        .wait_for_block_on_all(while_down, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.start_validator(0).await?;

    // The restarted node must replay, catch up to the moving tip, and agree.
    let target = cluster.max_height().await? + 3;
    cluster
        .wait_for_block_on_all(target, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(while_down).await?;

    // The sharp assertion: the batcher is alive *past its recovery* — new batches
    // land on L1 after the restart. Before the restart-replay fix the batcher task
    // panicked within seconds of startup and settlement stopped for good.
    wait_for_l1_state(
        cluster.node(0),
        "the restarted batcher commits new batches to L1",
        |state| state.last_committed_batch > before_restart.last_committed_batch,
    )
    .await?;

    cluster.shutdown_all().await
}

/// A three-validator simplex committee tolerates zero faults (quorum is 3-of-3):
/// stopping any validator pauses finalization, and restarting it resumes the chain.
/// This pins the availability boundary as an executable fact — deployments that need
/// to survive f faults must size the committee at n >= 3f+1, i.e. at least four.
#[test_log::test(tokio::test)]
async fn three_validator_chain_pauses_without_quorum_and_resumes() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::start(3).await?;

    cluster.stop_validator(2).await?;

    // Fully-formed finalizations may still deliver right after the stop; once those
    // settle, nothing new can finalize — a new certificate needs all three votes.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let frozen_at = cluster.max_height().await?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        cluster.max_height().await?,
        frozen_at,
        "the chain advanced without quorum",
    );

    // The committee is whole again: the chain must pick up where it froze.
    cluster.start_validator(2).await?;
    cluster
        .wait_for_block_on_all(frozen_at + 5, CONVERGENCE_TIMEOUT)
        .await?;

    cluster.shutdown_all().await
}

/// A transaction must reach a leader no matter which validator's RPC received it: the
/// committee gossips its mempools. The setup makes gossip the only possible carrier —
/// a nonce-gapped transaction (not yet includable) is submitted to one validator, that
/// validator dies, and only then is the gap filled through a different validator. The
/// gapped transaction's inclusion proves it left the dead validator's mempool over
/// gossip; nothing else ever had its bytes.
#[test_log::test(tokio::test)]
async fn gossiped_transaction_survives_its_receiving_validator() -> anyhow::Result<()> {
    let mut cluster = MultiNodeTester::start(4).await?;
    let sender = cluster.node(3).l2_wallet.default_signer().address();
    let base_nonce = cluster
        .node(3)
        .l2_provider
        .get_transaction_count(sender)
        .await?;

    // The gapped transaction: queued everywhere until `base_nonce` is used, so no
    // leader can include it while validator 3 is still alive. Gas is set explicitly —
    // estimation would simulate with the gapped nonce and refuse.
    let gapped = cluster
        .node(3)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::repeat_byte(0x61))
                .with_value(U256::from(1u64))
                .with_nonce(base_nonce + 1)
                .with_gas_limit(210_000),
        )
        .await?;
    let gapped_hash = *gapped.tx_hash();

    // Gossip carries it to the rest of the committee (visible in their pools).
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        if cluster
            .node(0)
            .l2_provider
            .get_transaction_by_hash(gapped_hash)
            .await?
            .is_some()
        {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "transaction did not gossip to other validators within {CONVERGENCE_TIMEOUT:?}",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The only validator whose RPC ever saw the transaction goes away...
    cluster.stop_validator(3).await?;

    // ...and once the gap is filled through a different validator, the gossiped copy
    // becomes includable and lands. The gap-filler's own receipt is not awaited —
    // the gapped transaction's receipt below proves both were included, in order.
    let _ = cluster
        .node(0)
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::repeat_byte(0x62))
                .with_value(U256::from(1u64))
                .with_nonce(base_nonce)
                .with_gas_limit(210_000),
        )
        .await?;

    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    let receipt = loop {
        if let Some(receipt) = cluster
            .node(0)
            .l2_provider
            .get_transaction_receipt(gapped_hash)
            .await?
        {
            break receipt;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "gossiped transaction was not included within {CONVERGENCE_TIMEOUT:?} \
             after its receiving validator died",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(receipt.status(), "gossiped transaction must succeed");

    let included_at = receipt
        .block_number
        .expect("included transactions have a block");
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(included_at).await?;

    cluster.shutdown_all().await
}
