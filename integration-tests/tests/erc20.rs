use alloy::eips::eip1559::Eip1559Estimation;
use alloy::network::TxSigner;
use alloy::primitives::{Address, B256, U256, address};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolValue;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::TestERC20::TestERC20Instance;
use zksync_os_integration_tests::contracts::{IL2AssetRouter, L1AssetRouter, TestERC20};
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;
use zksync_os_integration_tests::provider::ZksyncApi;
use zksync_os_types::{L2ToL1Log, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE, ZkTxType};

#[test_log::test(tokio::test)]
async fn erc20_deposit() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;
    let alice = tester.l1_wallet.default_signer().address();

    // Mint ERC20 tokens on L1 for Alice
    let mint_amount = U256::from(100u64);
    let deposit_amount = U256::from(40u64);
    let l1_erc20 = deploy_l1_token_and_mint(&tester, mint_amount).await?;

    let alice_l1_initial_balance = l1_erc20.balanceOf(alice).call().await?;
    assert_eq!(
        alice_l1_initial_balance, mint_amount,
        "Unexpected initial L1 balance"
    );

    // Send deposit.
    let l1_deposit_receipt = deposit_erc20(&tester, &l1_erc20, alice, deposit_amount).await?;

    // Check that the L2 part of the deposit was successful.
    assert_successful_deposit_l2_part(&tester, l1_deposit_receipt).await?;

    let l2_erc20_address = l2_token_address(&tester, *l1_erc20.address()).await?;
    // `l2_erc20` is not exactly `TestERC20`, but it implements standard `ERC20` interface.
    let l2_erc20 = TestERC20::new(l2_erc20_address, tester.l2_provider.clone());
    // Check Alice's L2 balance.
    let l2_balance = l2_erc20.balanceOf(alice).call().await?;

    assert_eq!(
        l2_balance, deposit_amount,
        "Unexpected L2 balance after deposit"
    );

    // Check Alice's L1 balance.
    let l1_balance = l1_erc20.balanceOf(alice).call().await?;
    assert_eq!(
        l1_balance,
        mint_amount - deposit_amount,
        "Unexpected L1 balance after deposit"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn erc20_transfer() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;
    // We use L2 wallet's default signer as Alice because it already has L2 ETH.
    let alice = tester.l2_wallet.default_signer().address();
    let bob_signer = PrivateKeySigner::random();
    let bob = bob_signer.address();

    // Deposit some ERC20 tokens to L2 for Alice to be able to transfer them.
    let mint_amount = U256::from(100u64);
    let l1_erc20 = deploy_l1_token_and_mint(&tester, mint_amount).await?;
    let l1_deposit_receipt = deposit_erc20(&tester, &l1_erc20, alice, mint_amount).await?;
    assert_successful_deposit_l2_part(&tester, l1_deposit_receipt).await?;

    let l2_erc20_address = l2_token_address(&tester, *l1_erc20.address()).await?;
    let l2_erc20 = TestERC20::new(l2_erc20_address, tester.l2_provider.clone());

    // Transfer some tokens from Alice to Bob on L2.
    let transfer_amount = U256::from(40u64);
    l2_erc20
        .transfer(bob, transfer_amount)
        .from(alice)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    // Check balances.
    let alice_l2_balance = l2_erc20.balanceOf(alice).call().await?;
    let bob_l2_balance = l2_erc20.balanceOf(bob).call().await?;

    assert_eq!(
        alice_l2_balance,
        mint_amount - transfer_amount,
        "Unexpected Alice's L2 balance after transfer"
    );

    assert_eq!(
        bob_l2_balance, transfer_amount,
        "Unexpected Bob's L2 balance after transfer"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn erc20_withdrawal() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;
    // We use L2 wallet's default signer as Alice because it already has L2 ETH.
    let alice = tester.l2_wallet.default_signer().address();

    // Deposit some ERC20 tokens to L2.
    let mint_amount = U256::from(100u64);
    let l1_erc20 = deploy_l1_token_and_mint(&tester, mint_amount).await?;
    let l1_deposit_receipt = deposit_erc20(&tester, &l1_erc20, alice, mint_amount).await?;
    assert_successful_deposit_l2_part(&tester, l1_deposit_receipt).await?;

    let l2_erc20_address = l2_token_address(&tester, *l1_erc20.address()).await?;
    let l2_erc20 = TestERC20::new(l2_erc20_address, tester.l2_provider.clone());
    let l2_asset_router_address = address!("0x0000000000000000000000000000000000010003");
    let l2_asset_router =
        IL2AssetRouter::new(l2_asset_router_address, tester.l2_zk_provider.clone());

    // Request withdrawal on L2.
    let withdraw_amount = U256::from(40u64);
    let l2_receipt = l2_asset_router
        .withdraw(alice, l2_erc20_address, withdraw_amount)
        .send()
        .await?
        .expect_to_execute()
        .await?;
    // Finalize withdrawal on L1.
    let l1_asset_router =
        L1AssetRouter::new(tester.l1_provider.clone(), tester.l2_zk_provider.clone()).await?;
    let l1_nullifier = l1_asset_router.l1_nullifier().await?;
    l1_nullifier.finalize_withdrawal(l2_receipt).await?;

    // Check balances.
    let l1_balance = l1_erc20.balanceOf(alice).call().await?;
    let l2_balance = l2_erc20.balanceOf(alice).call().await?;

    assert_eq!(
        l1_balance, withdraw_amount,
        "Unexpected Alice's L1 balance after withdrawal"
    );

    assert_eq!(
        l2_balance,
        mint_amount - withdraw_amount,
        "Unexpected Alice's L2 balance after withdrawal"
    );

    Ok(())
}

async fn deploy_l1_token_and_mint(
    tester: &Tester,
    mint_amount: U256,
) -> anyhow::Result<TestERC20Instance<EthDynProvider>> {
    let l1_erc20 = TestERC20::deploy(
        tester.l1_provider.clone(),
        U256::ZERO,
        "Test token".to_string(),
        "TEST".to_string(),
    )
    .await?;
    l1_erc20
        .mint(tester.l1_wallet.default_signer().address(), mint_amount)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    Ok(l1_erc20)
}

async fn deposit_erc20(
    tester: &Tester,
    l1_erc20: &TestERC20Instance<EthDynProvider>,
    to: Address,
    amount: U256,
) -> anyhow::Result<TransactionReceipt> {
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

    let l2_gas_limit = 1_500_000;
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            max_fee_per_gas + max_priority_fee_per_gas,
            l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
        )
        .await?;
    let shared_bridge_address = bridgehub.shared_bridge_address().await?;
    let second_bridge_calldata = (*l1_erc20.address(), amount, to).abi_encode();

    // Approve the bridge to spend Alice's L1 tokens.
    l1_erc20
        .approve(shared_bridge_address, amount)
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
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            to,
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

    Ok(l1_deposit_receipt)
}

async fn assert_successful_deposit_l2_part(
    tester: &Tester,
    l1_deposit_receipt: TransactionReceipt,
) -> anyhow::Result<()> {
    // Find the L1->L2 transaction hash in the deposit receipt logs.
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
            sender: address!("0x0000000000000000000000000000000000008001"),
            // Canonical tx hash
            key: l2_tx_hash,
            // Successful
            value: B256::from(U256::from(1)),
        },
        "expected L1->L2 deposit log to mark canonical tx hash as successful"
    );

    Ok(())
}

async fn l2_token_address(tester: &Tester, l1_token: Address) -> anyhow::Result<Address> {
    let l2_asset_router_address = address!("0x0000000000000000000000000000000000010003");
    let l2_asset_router = IL2AssetRouter::new(l2_asset_router_address, tester.l2_provider.clone());
    let l2_erc20_address = l2_asset_router.l2TokenAddress(l1_token).call().await?;

    Ok(l2_erc20_address)
}
