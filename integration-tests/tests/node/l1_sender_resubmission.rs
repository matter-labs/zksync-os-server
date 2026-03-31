use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, ReceiptAssert};
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

/// Verifies that when a submitted L1 transaction times out (because mining
/// is paused), the L1 sender resubmits it, and after mining resumes the
/// batch is eventually finalized.
///
/// Uses a 2 s `transaction_timeout` so the test completes in a few seconds
/// rather than waiting the production 300 s.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn resubmitted_tx_is_eventually_confirmed() -> anyhow::Result<()> {
    let tester = Tester::setup_with_overrides(|config| {
        // Short timeout so the Watcher fires a resubmission quickly.
        config.l1_sender_config.transaction_timeout = Duration::from_secs(2);
        config.sequencer_config.block_time = Duration::from_millis(100);
    })
    .await?;

    // Pause L1 auto-mining so submitted transactions sit in the mempool.
    tester
        .l1_provider()
        .raw_request::<_, ()>("anvil_setAutomine".into(), (false,))
        .await?;

    // Send one L2 transaction to trigger batch production and an L1 commit tx.
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    let l2_block = tester.l2_provider.get_block_number().await?;

    // Wait long enough for the Watcher to time out (2 s timeout + margin).
    // Sleep 5 s = 2 s timeout + 3 s margin to ensure the watcher fires.
    // TODO: once a resubmission metric or hook is added, assert it fired here
    //       rather than relying solely on eventual-finalization as the signal.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Re-enable mining — the resubmitted (or re-watched) tx should now mine.
    tester
        .l1_provider()
        .raw_request::<_, ()>("anvil_setAutomine".into(), (true,))
        .await?;

    // Mine a block to flush any pending transactions.
    tester
        .l1_provider()
        .raw_request::<_, ()>("anvil_mine".into(), (1u64, 1u64))
        .await?;

    // The node must eventually finalize the L2 block.
    tester
        .l2_zk_provider
        .wait_finalized_with_timeout(l2_block, DEFAULT_TIMEOUT)
        .await?;

    Ok(())
}
