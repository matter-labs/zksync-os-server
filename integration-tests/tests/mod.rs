use alloy::eips::eip2930::AccessList;
use alloy::network::{TransactionBuilder, TxSigner};
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{AccessListItem, TransactionRequest};
use tokio::time::Instant;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::{ReceiptAssert, ReceiptsAssert};

#[test_log::test(tokio::test)]
async fn basic_transfers() -> anyhow::Result<()> {
    // Test that the node can process 100 concurrent transfers to random accounts
    let tester = Tester::setup().await?;
    let alice = tester.l2_wallet.default_signer().address();
    let alice_balance_before = tester.l2_provider.get_balance(alice).await?;

    let deposit_amount = U256::from(100);
    let mut pending_txs = vec![];
    let start = Instant::now();
    for _ in 0..100 {
        let tx = TransactionRequest::default()
            .with_to(Address::random())
            .with_value(deposit_amount);
        pending_txs.push(tester.l2_provider.send_transaction(tx).await?);
    }
    tracing::info!(elapsed = ?start.elapsed(), "submitted all tx requests");

    let start = Instant::now();
    let receipts = pending_txs.expect_successful_receipts().await?;
    tracing::info!(elapsed = ?start.elapsed(), "resolved all tx receipts");

    let start = Instant::now();
    for receipt in receipts {
        let balance = tester.l2_provider.get_balance(receipt.to.unwrap()).await?;
        assert_eq!(balance, deposit_amount);
    }
    tracing::info!(elapsed = ?start.elapsed(), "confirmed final balances");

    // Alice should've lost at least `deposit_amount * 100` ETH
    let alice_balance_after = tester.l2_provider.get_balance(alice).await?;
    assert!(alice_balance_after < alice_balance_before - deposit_amount * U256::from(100));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn eip2930() -> anyhow::Result<()> {
    // Test that the node can process EIP-2930 transactions
    let tester = Tester::setup().await?;

    let tx = TransactionRequest::default()
        .from(tester.l2_wallet.default_signer().address())
        .to(Address::random())
        .value(U256::from(100))
        .access_list(AccessList(vec![AccessListItem::default()]));
    tester
        .l2_provider
        .send_transaction(tx)
        .await?
        .expect_successful_receipt()
        .await?;

    Ok(())
}
