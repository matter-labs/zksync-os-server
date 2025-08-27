use alloy::eips::eip1559::Eip1559Estimation;
use alloy::network::TxSigner;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::sol_types::SolValue;
use std::str::FromStr;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::TestERC20;
use zksync_os_integration_tests::provider::ZksyncApi;
use zksync_os_types::{L2ToL1Log, ZkTxType};

#[test_log::test(tokio::test)]
async fn erc20_deposit() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;
    let alice = tester.l1_wallet.default_signer().address();

    // Mint ERC20 tokens on L1 for Alice
    let mint_amount = U256::from(100u64);
    let deposit_amount = U256::from(50u64);
    let l1_erc20 = TestERC20::deploy(tester.l1_provider.clone()).await?;
    l1_erc20
        .mint(alice, mint_amount)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let alice_l1_initial_balance = l1_erc20.balanceOf(alice).call().await?;
    assert_eq!(
        alice_l1_initial_balance, mint_amount,
        "Unexpected initial L1 balance"
    );

    // todo: copied over from alloy-zksync, use directly once it is EIP-712 agnostic
    let bridgehub = Bridgehub::new(
        tester.l2_zk_provider.get_bridgehub_contract().await?,
        tester.l1_provider.clone(),
        270,
    );

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

    let l2_gas_limit = U256::from(500_000);
    let l2_gas_per_pubdata = U256::from(800);
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            U256::from(max_fee_per_gas + max_priority_fee_per_gas),
            l2_gas_limit,
            l2_gas_per_pubdata,
        )
        .await?;
    let shared_bridge_address = bridgehub.shared_bridge_address().await?;
    let second_bridge_calldata = (*l1_erc20.address(), deposit_amount, alice).abi_encode();

    // Approve the bridge to spend Alice's L1 tokens.
    l1_erc20
        .approve(shared_bridge_address, deposit_amount)
        .from(alice)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    // Prepare deposit request.
    let deposit_request = bridgehub
        .request_l2_transaction_two_bridges(
            tx_base_cost,
            U256::ZERO,
            l2_gas_limit,
            l2_gas_per_pubdata,
            alice,
            shared_bridge_address,
            U256::ZERO,
            second_bridge_calldata,
        )
        .value(tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();

    // Send deposit request and wait for it to be processed on L2.
    let l1_deposit_receipt = tester
        .l1_provider
        .send_transaction(deposit_request)
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

    Ok(())
}
