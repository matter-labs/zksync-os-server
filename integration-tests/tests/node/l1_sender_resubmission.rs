use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::providers::ext::AnvilApi;
use alloy::rpc::types::TransactionRequest;
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
/// 4. Wait long enough for the timeout to fire at least once and a replacement transaction
///    to be submitted.
/// 5. Re-enable L1 mining.
/// 6. Wait for the L2 block to be finalized on L1, confirming the pipeline recovered.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn l1_sender_resubmits_after_timeout() -> anyhow::Result<()> {
    // A very short timeout to make the test fast.  The node will re-evaluate after 5 s.
    const TX_TIMEOUT: Duration = Duration::from_secs(5);

    let tester = Tester::setup_with_overrides(|config| {
        config.l1_sender_config.transaction_timeout = TX_TIMEOUT;
        // Fast block production so we get a batch quickly.
        config.sequencer_config.block_time = Duration::from_millis(200);
    })
    .await?;

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

    // Give the node time to submit the first L1 transaction and for the timeout to fire
    // at least once.  Two full timeout periods is enough.
    tokio::time::sleep(TX_TIMEOUT * 2).await;

    // Re-enable auto-mining.  Any replacement transactions submitted during the timeout
    // window will now be included.
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

    // Wait for the block to be finalized (executed) on L1.  If resubmission works the
    // pipeline should complete within DEFAULT_TIMEOUT from this point.
    tester
        .l2_zk_provider
        .wait_finalized_with_timeout(l2_block, DEFAULT_TIMEOUT)
        .await?;

    Ok(())
}
