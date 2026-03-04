use alloy::eips::Encodable2718;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U128, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use futures::FutureExt;
use std::time::Duration;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::dyn_wallet_provider::EthWalletProvider;
use zksync_os_server::config::FeeConfig;

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
    let gas_limit = 100_000;
    let value = U256::from(100);
    // Prepare Bob's transaction with a nonce gap
    let bob_tx = TransactionRequest::default()
        .with_from(bob)
        .with_to(Address::random())
        .with_value(value)
        .with_gas_price(gas_price)
        .with_gas_limit(gas_limit)
        .with_nonce(1);

    // This is what it will cost to execute Bob's legacy transaction
    let bob_tx_cost = U256::from(gas_limit) * U256::from(gas_price) + value;
    // Since bob doesn't have enough, mempool should reject the transaction
    let error = tester
        .l2_provider
        .send_transaction(bob_tx.clone())
        .await
        .expect_err("sending transaction should fail");
    assert!(
        error
            .to_string()
            .contains("sender does not have enough funds")
    );

    // Alice gives Bob enough for his transaction
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_from(alice)
                .with_to(bob)
                .with_value(bob_tx_cost),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    // Now mempool should accept Bob's transaction
    let bob_receipt_fut = tester
        .l2_provider
        .send_transaction(bob_tx)
        .await?
        .expect_successful_receipt()
        .map(|res| res.expect("transaction should be successful"))
        .shared();

    // But then Bob spends all of his funds on another transaction; note that nonce here is 0
    tester
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
        .expect_successful_receipt()
        .await?;
    // Bob's second transaction is unminable because of the lack of funds in Bob's account
    tokio::time::timeout(std::time::Duration::from_secs(3), bob_receipt_fut.clone())
        .await
        .expect_err("transaction should timeout");

    // Alice gives Bob enough for his second transaction
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_from(alice)
                .with_to(bob)
                .with_value(bob_tx_cost),
        )
        .await?
        .expect_successful_receipt()
        .await?;
    // Bob's second transaction should be minable now
    bob_receipt_fut.await;

    Ok(())
}

/// A transaction with maxFeePerGas below the chain's base fee must not stall
/// block production for other senders.
#[test_log::test(tokio::test)]
async fn low_fee_tx_does_not_hang_block_executor() -> anyhow::Result<()> {
    // Use a deterministic base fee so the "low fee" value is unambiguous.
    let known_base_fee: u128 = 100_000_000; // 100M wei = 0.1 gwei
    let fee_config = FeeConfig {
        native_price_usd: 3e-9,
        base_fee_override: Some(U128::from(known_base_fee)),
        native_per_gas: 100,
        pubdata_price_override: Some(U128::from(1_000_000u64)),
        native_price_override: Some(U128::from(1_000_000u64)),
        pubdata_price_cap: None,
    };
    let mut tester = Tester::builder()
        .fee_config(fee_config)
        .block_time(Duration::from_millis(500))
        .build()
        .await?;

    let alice = tester.l2_wallet.default_signer().address();
    let chain_id = tester.l2_provider.get_chain_id().await?;

    // Step 1: Confirm baseline — chain is working
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    // Step 2: Create Bob (independent sender) and fund him from Alice
    let bob_signer = PrivateKeySigner::random();
    let bob = bob_signer.address();
    tester.l2_provider.wallet_mut().register_signer(bob_signer);
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_from(alice)
                .with_to(bob)
                .with_value(U256::from(10u64.pow(18))), // 1 ETH
        )
        .await?
        .expect_successful_receipt()
        .await?;

    // Step 3: Submit a low-fee tx from Alice with maxFeePerGas=7 (far below base fee of 100M).
    // Uses build() + send_raw_transaction() to bypass provider fee estimation.
    let nonce = tester.l2_provider.get_transaction_count(alice).await?;
    let poison_tx = TransactionRequest::default()
        .with_to(Address::random())
        .with_value(U256::from(1))
        .with_nonce(nonce)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(7) // Above Reth MIN_PROTOCOL_BASE_FEE, far below actual base fee
        .with_max_priority_fee_per_gas(0)
        .with_chain_id(chain_id);
    let poison_envelope = poison_tx.build(&tester.l2_wallet).await?;
    let poison_encoded = poison_envelope.encoded_2718();
    let _ = tester
        .l2_provider
        .send_raw_transaction(&poison_encoded)
        .await?;

    // Give the block executor time to pick up the low-fee tx
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Step 4: Send a legitimate follow-up from Bob (independent sender, no nonce dependency).
    let follow_up_tx = TransactionRequest::default()
        .with_from(bob)
        .with_to(Address::random())
        .with_value(U256::from(1));

    let result = tokio::time::timeout(Duration::from_secs(30), async {
        tester
            .l2_provider
            .send_transaction(follow_up_tx)
            .await?
            .expect_successful_receipt()
            .await
    })
    .await;

    match result {
        Ok(Ok(_receipt)) => {
            // Block executor handled the low-fee tx gracefully — test passes
        }
        Ok(Err(e)) => {
            panic!("Follow-up transaction failed unexpectedly: {e:#}");
        }
        Err(_elapsed) => {
            panic!(
                "Follow-up transaction not mined within 30s. \
                 The low-fee tx (maxFeePerGas=7, baseFee={known_base_fee}) \
                 appears to have stalled block production for other senders."
            );
        }
    }

    Ok(())
}
