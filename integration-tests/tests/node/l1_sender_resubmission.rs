use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::providers::ext::{AnvilApi, TxPoolApi};
use alloy::rpc::types::TransactionRequest;
use smart_config::EtherAmount;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, ReceiptAssert};
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

/// Exercises the per-transaction resubmission loop:
///
/// 1. Start the node with a very short `transaction_timeout` (5 s) so the timeout fires
///    quickly in the test.
/// 2. Disable L1 auto-mining so the first submitted transaction is never included.
/// 3. Send an L2 transaction to trigger the batcher pipeline (commit → prove → execute).
/// 4. Wait for the commit tx to appear in the mempool, then wait one more TX_TIMEOUT window
///    so the node resubmits it with bumped fees.
/// 5. Raise the L1 base fee above the original tx's max fee — making it unmineable —
///    then re-enable L1 mining.  Only the replacement (with ≥10% higher fees) can be mined.
/// 6. Wait for the L2 block to be finalized on L1.  Finalization proves the replacement
///    was mined: the original couldn't have been included at the elevated base fee.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn l1_sender_resubmits_after_timeout() -> anyhow::Result<()> {
    // A very short timeout to make the test fast.  The node will re-evaluate after 5 s.
    const TX_TIMEOUT: Duration = Duration::from_secs(5);

    let tester = Tester::setup_with_overrides(|config| {
        config.l1_sender_config.transaction_timeout = TX_TIMEOUT;
        // Raise fee caps well above what Anvil reports so fee-cap gating does not
        // prevent transaction submission during the test.
        config.l1_sender_config.max_priority_fee_per_gas = EtherAmount(10 * 1_000_000_000); // 10 gwei
        config.l1_sender_config.max_fee_per_gas = EtherAmount(500 * 1_000_000_000); // 500 gwei
        // Fast block production so we get a batch quickly.
        config.sequencer_config.block_time = Duration::from_millis(200);
    })
    .await?;

    // Capture the current fee estimate before pausing mining.  The original commit tx
    // will be submitted at approximately these fees; we use `max_fee_per_gas` to set
    // the next block's base fee just above that value, making the original tx unmineable
    // while the replacement (with ≥10% bumped fees) remains valid.
    let initial_fees = tester.l1_provider().estimate_eip1559_fees().await?;

    // Stop auto-mining so L1 transactions stay pending indefinitely.
    tester
        .l1_provider()
        .anvil_set_auto_mine(false)
        .await
        .expect("anvil_set_auto_mine(false)");

    // Submit an L2 transaction.  This will eventually be batched and trigger the commit
    // sender to submit an L1 transaction (which will time out because mining is paused).
    let receipt = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;
    let l2_block = receipt
        .block_number
        .expect("receipt must have a block number");

    // Wait until at least one L1 transaction is pending in the mempool.
    // The pipeline (produce blocks → seal batch → build commit tx) takes several
    // seconds on a fresh node; polling here starts the TX_TIMEOUT clock only
    // once the first commit tx is actually queued in the L1 mempool.
    {
        const PIPELINE_TIMEOUT: Duration = Duration::from_secs(60);
        let deadline = tokio::time::Instant::now() + PIPELINE_TIMEOUT;
        loop {
            let status = tester.l1_provider().txpool_status().await?;
            if status.pending > 0 {
                break;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "no L1 tx submitted within {PIPELINE_TIMEOUT:?}",
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // The commit tx is now in the mempool.  Sleep for one TX_TIMEOUT window plus a
    // small buffer so the timeout fires once and the replacement is submitted, but
    // not long enough for a second timeout to fire and collide with mining.
    tokio::time::sleep(TX_TIMEOUT + Duration::from_secs(2)).await;

    // Raise the base fee just above the original transaction's max fee so that
    // only the replacement (which has fees bumped ≥10%) is valid for the next block.
    // Anvil uses FIFO ordering, so without this the original tx would be mined first.
    tester
        .l1_provider()
        .anvil_set_next_block_base_fee_per_gas(initial_fees.max_fee_per_gas + 1)
        .await
        .expect("anvil_set_next_block_base_fee_per_gas");

    // Re-enable auto-mining.  The replacement transaction will be included; the
    // original is excluded because its max_fee_per_gas is below the new base fee.
    tester
        .l1_provider()
        .anvil_set_auto_mine(true)
        .await
        .expect("anvil_set_auto_mine(true)");

    // Mine a few blocks to ensure pending transactions are included promptly.
    tester
        .l1_provider()
        .anvil_mine(Some(3u64), None)
        .await
        .expect("anvil_mine");

    // Wait for the block to be finalized (executed) on L1.  Successful finalization
    // proves that resubmission worked: the pipeline completed even though the first
    // commit tx was never mined, and the replacement (the only valid tx at the elevated
    // base fee) carried the batch through.
    tester
        .l2_zk_provider
        .wait_finalized_with_timeout(l2_block, DEFAULT_TIMEOUT)
        .await?;

    Ok(())
}
