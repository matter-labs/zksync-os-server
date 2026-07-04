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
