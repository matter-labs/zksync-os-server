use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use zksync_integration_tests::Tester;

#[test_log::test(tokio::test)]
async fn foo() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;

    let tx = TransactionRequest::default()
        .with_to(Address::random())
        .with_value(U256::from(100));
    let receipt = tester
        .l2_provider
        .send_transaction(tx)
        .await?
        .get_receipt()
        .await?;
    assert!(receipt.status(), "transaction failed");

    Ok(())
}
