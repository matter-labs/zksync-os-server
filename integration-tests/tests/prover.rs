#![cfg(feature = "prover-tests")]

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use backon::ConstantBuilder;
use backon::Retryable;
use std::time::Duration;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;

#[test_log::test(tokio::test)]
async fn prover() -> anyhow::Result<()> {
    // Test that the node can run `eth_call` on genesis
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

    (|| async {
        let status = tester.prover_api.check_batch_status(1).await?;
        if status {
            Ok(())
        } else {
            Err(anyhow::anyhow!("batch is not ready yet"))
        }
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_secs(30))
            .with_max_times(40),
    )
    .notify(|err: &anyhow::Error, dur: Duration| {
        tracing::info!(?err, ?dur, "proof not ready yet, retrying");
    })
    .await?;

    Ok(())
}
