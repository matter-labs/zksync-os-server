use alloy::eips::eip1559::Eip1559Estimation;
use alloy::eips::eip2930::AccessList;
use alloy::network::{TransactionBuilder, TxSigner};
use alloy::primitives::{Address, U256};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::rpc::types::{AccessListItem, TransactionRequest};
use std::str::FromStr;
use tokio::time::Instant;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;

#[test_log::test(tokio::test)]
async fn basic_transfers() -> anyhow::Result<()> {
    // Test that the node can process 100 concurrent transfers to random accounts
    let tester = Tester::setup().await?;
    let alice = tester.l2_wallet.default_signer().address();
    let alice_balance_before = tester.l2_provider.get_balance(alice).await?;

    let deposit_amount = U256::from(100);
    let mut receipt_futures = vec![];
    let start = Instant::now();
    for _ in 0..100 {
        let tx = TransactionRequest::default()
            .with_to(Address::random())
            .with_value(deposit_amount);
        let receipt_future = tester
            .l2_provider
            .send_transaction(tx)
            .await?
            .expect_successful_receipt();
        receipt_futures.push(receipt_future);
    }
    tracing::info!(elapsed = ?start.elapsed(), "submitted all tx requests");

    let start = Instant::now();
    let receipts = futures::future::join_all(receipt_futures)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
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
async fn l1_deposit() -> anyhow::Result<()> {
    // Test that we can deposit L2 funds from a rich L1 account
    let tester = Tester::setup().await?;
    let alice = tester.l1_wallet.default_signer().address();
    let alice_l1_initial_balance = tester.l1_provider.get_balance(alice).await?;
    let alice_l2_initial_balance = tester.l2_provider.get_balance(alice).await?;

    // todo: copied over from alloy-zksync, use directly once it is EIP-712 agnostic
    let bridgehub = Bridgehub::new(
        Address::from_str("0x4b37536b9824c4a4cf3d15362135e346adb7cb9c").unwrap(),
        tester.l1_provider.clone(),
        270,
    );
    let amount = U256::from(100);
    let gas_limit = U256::from(500_000);
    let gas_per_pubdata = U256::from(800);
    let max_priority_fee_per_gas = tester.l1_provider.get_max_priority_fee_per_gas().await?;
    let base_l1_fees_data = tester
        .l1_provider
        .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
            Eip1559Estimation {
                max_fee_per_gas: base_fee_per_gas * 3 / 2,
                max_priority_fee_per_gas: 0,
            }
        }))
        .await?;
    let max_fee_per_gas = base_l1_fees_data.max_fee_per_gas + max_priority_fee_per_gas;
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            U256::from(max_fee_per_gas + max_priority_fee_per_gas),
            gas_limit,
            gas_per_pubdata,
        )
        .await?;
    let l1_deposit_request = bridgehub
        .request_l2_transaction_direct(
            amount + tx_base_cost,
            alice,
            amount,
            vec![],
            gas_limit,
            gas_per_pubdata,
            alice,
        )
        .value(amount + tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();
    let l1_deposit_receipt = tester
        .l1_provider
        .send_transaction(l1_deposit_request)
        .await?
        .expect_successful_receipt()
        .await?;
    let l1_to_l2_tx_log = l1_deposit_receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .expect("no L1->L2 logs produced by deposit tx");
    let l2_tx_hash = l1_to_l2_tx_log.inner.txHash;

    {
        // fixme: temporarily submit a random L2 transaction to make sequencer pick up new L1 tx
        let tx = TransactionRequest::default()
            .with_to(Address::random())
            .with_value(U256::from(100));
        tester
            .l2_provider
            .send_transaction(tx)
            .await?
            .expect_successful_receipt()
            .await?;
    }

    PendingTransactionBuilder::new(tester.l2_provider.root().clone(), l2_tx_hash)
        .expect_successful_receipt()
        .await?;
    let fee = U256::from(l1_deposit_receipt.effective_gas_price)
        * U256::from(l1_deposit_receipt.gas_used);

    let alice_l1_final_balance = tester.l1_provider.get_balance(alice).await?;
    let alice_l2_final_balance = tester.l2_provider.get_balance(alice).await?;
    assert!(alice_l1_final_balance <= alice_l1_initial_balance - fee - amount);
    assert!(alice_l2_final_balance >= alice_l2_initial_balance + amount);

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
