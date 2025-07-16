use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use futures::FutureExt;
use zksync_os_integration_tests::dyn_wallet_provider::EthWalletProvider;
use zksync_os_integration_tests::Tester;

#[test_log::test(tokio::test)]
async fn sensitive_to_balance_changes() -> anyhow::Result<()> {
    // Test that mempool gets notified when an account's balance changes, hence potentially
    // making that account's queued transactions minable.
    let mut tester = Tester::setup().await?;
    // Alice is a rich account
    let alice = tester.l2_wallet.default_signer().address();
    // Bob is an account with zero funds
    let bob_signer = PrivateKeySigner::random();
    let bob = bob_signer.address();
    tester
        .l2_provider
        .wallet_mut()
        .register_signer(bob_signer.clone());
    // Make sure Bob has no funds at the start
    assert_eq!(tester.l2_provider.get_balance(bob).await?, U256::ZERO);

    let gas_price = tester.l2_provider.get_gas_price().await?;
    let value = U256::from(100);
    // Prepare Bob's transaction with a nonce gap
    let bob_tx = TransactionRequest::default()
        .with_from(bob)
        .with_to(Address::random())
        .with_value(value)
        .with_gas_price(gas_price)
        .with_nonce(1);
    let gas_limit = tester.l2_provider.estimate_gas(bob_tx.clone()).await?;

    // This is what it will cost to execute Bob's legacy transaction
    let bob_tx_cost = U256::from(gas_limit) * U256::from(gas_price) + value;
    // Since bob doesn't have enough, mempool should reject the transaction
    let error = tester
        .l2_provider
        .send_transaction(bob_tx.clone())
        .await
        .expect_err("sending transaction should fail");
    assert!(error
        .to_string()
        .contains("sender does not have enough funds"));

    // Alice gives Bob enough for his transaction
    let receipt = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_from(alice)
                .with_to(bob)
                .with_value(bob_tx_cost),
        )
        .await?
        .get_receipt()
        .await?;
    assert!(receipt.status(), "transaction should be successful");

    // Now mempool should accept Bob's transaction
    let bob_receipt_fut = tester
        .l2_provider
        .send_transaction(bob_tx)
        .await?
        .get_receipt()
        .map(|res| res.expect("transaction should be successful"))
        .shared();

    // But then Bob spends all of his funds on another transaction; note that nonce here is 0
    let receipt = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_from(bob)
                .with_to(Address::random())
                .with_value(value)
                .with_gas_price(gas_price)
                .with_nonce(0),
        )
        .await?
        .get_receipt()
        .await?;
    assert!(receipt.status(), "transaction should be successful");
    // Bob's second transaction is unminable because of the lack of funds in Bob's account
    tokio::time::timeout(std::time::Duration::from_secs(3), bob_receipt_fut.clone())
        .await
        .expect_err("transaction should timeout");

    // Alice gives Bob enough for his second transaction
    let receipt = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_from(alice)
                .with_to(bob)
                .with_value(bob_tx_cost),
        )
        .await?
        .get_receipt()
        .await?;
    assert!(receipt.status(), "transaction should be successful");
    // Bob's second transaction should be minable now
    let receipt = bob_receipt_fut.await;
    assert!(receipt.status(), "transaction should be successful");

    Ok(())
}
