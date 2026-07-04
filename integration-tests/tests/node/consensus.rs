//! BFT consensus over real nodes: a 3-validator committee of full in-process nodes over
//! one L1, producing, verifying, and finalizing real blocks.
//!
//! Reaching the first assertion already proves a lot: node startup waits for the initial
//! L1 deposit to be *included in a block*, which under consensus requires the committee
//! to form over p2p, a leader to build a block carrying the L1 priority transaction,
//! the other validators to re-execute and vote for it, and the finalized block to flow
//! through every node's persistence pipeline.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::multi_node::MultiNodeTester;

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(60);

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
