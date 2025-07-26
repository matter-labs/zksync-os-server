#![cfg(feature = "prover-tests")]

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;

#[test_log::test(tokio::test)]
async fn prover() -> anyhow::Result<()> {
    // Test that prover can successfully prove at least one batch
    let tester = Tester::builder().enable_prover().build().await?;
    let tx = TransactionRequest::default()
        .with_to(Address::random())
        .with_value(U256::from(100));
    tester
        .l2_provider
        .send_transaction(tx)
        .await?
        .expect_successful_receipt()
        .await?;

    tester.prover_api.wait_for_batch_proven(1).await?;

    Ok(())
}
