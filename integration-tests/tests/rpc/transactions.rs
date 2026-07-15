use alloy::eips::eip2930::AccessList;
use alloy::network::{TransactionBuilder, TxSigner};
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{AccessListItem, TransactionRequest};
use tokio::time::Instant;
use zksync_os_integration_tests::assert_traits::{ReceiptAssert, ReceiptsAssert};
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

#[test_multisetup([CURRENT_TO_L1])]
async fn basic_transfers(tester: Tester) -> anyhow::Result<()> {
    // Test that the node can process 100 concurrent transfers to random accounts
    let alice = tester.l2_wallet.default_signer().address();
    let alice_balance_before = tester.l2_provider.get_balance(alice).await?;

    let deposit_amount = U256::from(100);
    let mut pending_txs = vec![];
    let start = Instant::now();
    // Give 10x buffer for gas price to ensure transactions do not get stuck in mempool in the
    // middle of execution.
    let gas_price = tester.l2_provider.get_gas_price().await? * 10;
    for _ in 0..100 {
        let tx = TransactionRequest::default()
            .with_to(Address::random())
            .with_value(deposit_amount)
            .with_gas_price(gas_price);
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

/// A deployment whose constructor reverts is still a valid transaction: with
/// the gas limit set explicitly (so client-side estimation cannot intercept
/// it), the node must include it in a block with a failed receipt and consume
/// the nonce — Ethereum semantics. This is the raw-submitter path SDK flows
/// never exercise, because their pre-send estimation surfaces the revert
/// before anything reaches the pool.
#[test_multisetup([CURRENT_TO_L1])]
async fn reverting_deployment_is_included_with_a_failed_receipt(
    tester: Tester,
) -> anyhow::Result<()> {
    let sender = tester.l2_wallet.default_signer().address();
    let nonce_before = tester.l2_provider.get_transaction_count(sender).await?;

    // Init code that always reverts: PUSH1 0, PUSH1 0, REVERT.
    let tx = TransactionRequest::default()
        .from(sender)
        .with_deploy_code([0x60, 0x00, 0x60, 0x00, 0xfd])
        .with_gas_limit(1_000_000)
        .with_gas_price(tester.l2_provider.get_gas_price().await? * 10);
    let receipt = tester
        .l2_provider
        .send_transaction(tx)
        .await?
        .get_receipt()
        .await?;

    assert!(!receipt.status(), "the constructor revert must surface");
    let nonce_after = tester.l2_provider.get_transaction_count(sender).await?;
    assert_eq!(
        nonce_after,
        nonce_before + 1,
        "an included-but-reverted deployment consumes the nonce"
    );
    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn eip2930(tester: Tester) -> anyhow::Result<()> {
    // Test that the node can process EIP-2930 transactions
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
