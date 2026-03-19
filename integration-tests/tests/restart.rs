use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn node_stop_and_restart_preserves_state() -> anyhow::Result<()> {
    let tester = Tester::builder().build().await?;

    // Send a transaction and wait for it to be included.
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
    let tx_hash = receipt.transaction_hash;

    // Restart the same node (same DB, same L1).
    let restarted = tester.restart().await?;
    // Wait for receipt's block to be available. It might not be immediately available because
    // repository DB did not persist the receipt during previous run.
    restarted
        .l2_zk_provider
        .wait_for_block(receipt.block_number.unwrap())
        .await?;

    // The transaction sent before the restart must still be retrievable.
    let recovered = restarted
        .l2_provider
        .get_transaction_receipt(tx_hash)
        .await?
        .expect("transaction receipt should be present after restart");
    assert_eq!(recovered.transaction_hash, tx_hash);

    Ok(())
}
