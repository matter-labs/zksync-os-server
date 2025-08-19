use alloy::eips::eip1559::Eip1559Estimation;
use alloy::network::TxSigner;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{PendingTransactionBuilder, Provider};
use std::str::FromStr;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::provider::ZksyncApi;
use zksync_os_types::{L2ToL1Log, ZkTxType};

#[test_log::test(tokio::test)]
async fn l1_deposit() -> anyhow::Result<()> {
    // Test that we can deposit L2 funds from a rich L1 account
    let tester = Tester::setup().await?;
    let alice = tester.l1_wallet.default_signer().address();
    let alice_l1_initial_balance = tester.l1_provider.get_balance(alice).await?;
    let alice_l2_initial_balance = tester.l2_provider.get_balance(alice).await?;

    // todo: copied over from alloy-zksync, use directly once it is EIP-712 agnostic
    let bridgehub = Bridgehub::new(
        tester.l2_zk_provider.get_bridgehub_contract().await?,
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

    let receipt = PendingTransactionBuilder::new(tester.l2_zk_provider.root().clone(), l2_tx_hash)
        .expect_successful_receipt()
        .await?;
    assert_eq!(
        receipt.inner.tx_type(),
        ZkTxType::L1,
        "expected L1->L2 deposit to produce an L1->L2 priority transaction"
    );
    let mut l2_to_l1_logs = receipt.inner.l2_to_l1_logs().to_vec();
    assert_eq!(
        l2_to_l1_logs.len(),
        1,
        "expected L1->L2 deposit transaction to only produce one L2->L1 log"
    );
    let l2_to_l1_log = l2_to_l1_logs.remove(0);
    assert_eq!(
        l2_to_l1_log,
        L2ToL1Log {
            // L1Messenger's address
            l2_shard_id: 0,
            is_service: true,
            tx_number_in_block: receipt.transaction_index.unwrap() as u16,
            sender: Address::from_str("0x0000000000000000000000000000000000008001").unwrap(),
            // Canonical tx hash
            key: l2_tx_hash,
            // Successful
            value: B256::from(U256::from(1)),
        },
        "expected L1->L2 deposit log to mark canonical tx hash as successful"
    );

    let fee = U256::from(l1_deposit_receipt.effective_gas_price)
        * U256::from(l1_deposit_receipt.gas_used);

    let alice_l1_final_balance = tester.l1_provider.get_balance(alice).await?;
    let alice_l2_final_balance = tester.l2_provider.get_balance(alice).await?;
    assert!(alice_l1_final_balance <= alice_l1_initial_balance - fee - amount);
    assert!(alice_l2_final_balance >= alice_l2_initial_balance + amount);

    Ok(())
}

// #[test_log::test(tokio::test)]
// async fn l1_withdraw() -> anyhow::Result<()> {
//     // Test that we can withdraw L2 funds to L1
//     let tester = Tester::setup().await?;
//     let alice = tester.l2_wallet.default_signer().address();
//     let alice_l1_initial_balance = tester.l1_provider.get_balance(alice).await?;
//     let alice_l2_initial_balance = tester.l2_provider.get_balance(alice).await?;
//     let amount = U256::from(100);
//
//     let l2_base_token = L2BaseToken::new(tester.l2_zk_provider.clone());
//     let withdrawal_l2_receipt = l2_base_token
//         .withdraw(alice, amount)
//         .await?
//         .expect_to_execute()
//         .await?;
//     let l2_fee = U256::from(
//         withdrawal_l2_receipt.effective_gas_price() * withdrawal_l2_receipt.gas_used() as u128,
//     );
//
//     let l1_asset_router =
//         L1AssetRouter::new(tester.l1_provider.clone(), tester.l2_zk_provider.clone()).await?;
//     let l1_nullifier = l1_asset_router.l1_nullifier().await?;
//     let finalize_withdrawal_l1_receipt = l1_nullifier
//         .finalize_withdrawal(withdrawal_l2_receipt)
//         .await?;
//     let l1_fee = U256::from(
//         finalize_withdrawal_l1_receipt.effective_gas_price()
//             * finalize_withdrawal_l1_receipt.gas_used() as u128,
//     );
//
//     let alice_l1_final_balance = tester.l1_provider.get_balance(alice).await?;
//     let alice_l2_final_balance = tester.l2_provider.get_balance(alice).await?;
//     assert!(alice_l1_final_balance >= alice_l1_initial_balance - l1_fee + amount);
//     assert!(alice_l2_final_balance <= alice_l2_initial_balance - l2_fee - amount);
//
//     Ok(())
// }
