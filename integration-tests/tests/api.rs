use alloy::eips::Encodable2718;
use alloy::network::{ReceiptResponse, TransactionBuilder, TxSigner};
use alloy::primitives::{Address, B256, TxHash, U128, U256, address};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, TransactionRequest};
use alloy::sol_types::SolEvent;
use anyhow::Context as _;
use regex::Regex;
use std::time::Duration;
use zksync_os_contract_interface::IExecutor::BlockCommit;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::Counter::CounterInstance;
use zksync_os_integration_tests::contracts::{Counter, EventEmitter};
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;
use zksync_os_integration_tests::provider::ZksyncApi;
use zksync_os_rpc_api::types::BatchStorageProof;
use zksync_os_server::config::FeeConfig;

#[test_log::test(tokio::test)]
async fn get_code() -> anyhow::Result<()> {
    // Test that the node:
    // * can fetch deployed bytecode at the latest block
    // * can fetch deployed bytecode at the block where it was deployed
    // * cannot fetch deployed bytecode before the block where it was deployed
    let tester = Tester::setup().await?;

    let deploy_tx_receipt = EventEmitter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");

    let latest_code = tester.l2_provider.get_code_at(contract_address).await?;
    assert_eq!(
        latest_code,
        EventEmitter::DEPLOYED_BYTECODE,
        "deployed bytecode mismatch at latest block"
    );
    let at_block_code = tester
        .l2_provider
        .get_code_at(contract_address)
        .block_id(
            deploy_tx_receipt
                .block_hash
                .expect("deploy receipt has no block hash")
                .into(),
        )
        .await?;
    assert_eq!(
        at_block_code,
        EventEmitter::DEPLOYED_BYTECODE,
        "deployed bytecode mismatch at deployed block"
    );
    let before_block_code = tester
        .l2_provider
        .get_code_at(contract_address)
        .block_id(
            (deploy_tx_receipt
                .block_number
                .expect("deploy receipt has no block number")
                - 1)
            .into(),
        )
        .await?;
    assert!(
        before_block_code.is_empty(),
        "deployed bytecode is not empty before deploy block"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn get_transaction_count() -> anyhow::Result<()> {
    // Test that the node takes pending mempool transactions into account for `eth_getTransactionCount`
    // We set block time to 5 seconds to make sure that transaction spends >5 seconds in the mempool.
    // This gives us time to check that the node returns the correct transaction count.
    let tester = Tester::builder()
        .block_time(Duration::from_secs(5))
        .build()
        .await?;
    let alice = tester.l2_wallet.default_signer().address();
    let l2_provider = &tester.l2_provider;

    // No existing transactions yet at the start
    assert_eq!(l2_provider.get_transaction_count(alice).await?, 0);

    let deploy_pending_tx = EventEmitter::deploy_builder(l2_provider.clone())
        .send()
        .await?;
    // Pending transaction count takes pending transaction into account, so it's 1
    assert_eq!(l2_provider.get_transaction_count(alice).pending().await?, 1);
    // Latest transaction count is still 0
    assert_eq!(l2_provider.get_transaction_count(alice).latest().await?, 0);
    // Omitting block id defaults to latest block
    assert_eq!(l2_provider.get_transaction_count(alice).await?, 0);

    // Wait for the transaction to be mined and check that the transaction count is 1 now
    deploy_pending_tx.expect_successful_receipt().await?;
    assert_eq!(l2_provider.get_transaction_count(alice).pending().await?, 1);
    assert_eq!(l2_provider.get_transaction_count(alice).latest().await?, 1);

    Ok(())
}

#[test_log::test(tokio::test)]
async fn get_net_version() -> anyhow::Result<()> {
    // Test that the node returns correct chain ID in `net_version` RPC call
    let tester = Tester::setup().await?;
    let net_version = tester.l2_provider.get_net_version().await?;
    let chain_id = tester.l2_provider.get_chain_id().await?;
    assert_eq!(net_version, chain_id);
    Ok(())
}

#[test_log::test(tokio::test)]
async fn get_client_version() -> anyhow::Result<()> {
    // Test that the node returns sensible value in `web3_clientVersion` RPC call
    let tester = Tester::setup().await?;
    let client_version = tester.l2_provider.get_client_version().await?;
    let regex = Regex::new(r"^zksync-os/v(\d+)\.(\d+)\.(\d+)")?;
    assert!(regex.is_match(&client_version));
    Ok(())
}

#[test_log::test(tokio::test)]
async fn get_gas_price_uses_configured_scale_factor() -> anyhow::Result<()> {
    let known_base_fee: u128 = 100_000_000;
    let fee_config = FeeConfig {
        native_price_usd: 3e-9,
        base_fee_override: Some(U128::from(known_base_fee)),
        native_per_gas: 100,
        pubdata_price_override: Some(U128::from(1_000_000u64)),
        native_price_override: Some(U128::from(1_000_000u64)),
        pubdata_price_cap: None,
    };
    let tester = Tester::builder()
        .fee_config(fee_config)
        .gas_price_scale_factor(2.0)
        .build()
        .await?;

    let gas_price = tester.l2_provider.get_gas_price().await?;
    assert_eq!(gas_price, 200_000_000);

    Ok(())
}

#[test_log::test(tokio::test)]
async fn send_raw_transaction_sync() -> anyhow::Result<()> {
    // Test that the node supports `eth_sendRawTransactionSync`
    let tester = Tester::builder().build().await?;

    let alice = tester.l2_wallet.default_signer().address();
    let fees = tester.l2_provider.estimate_eip1559_fees().await?;
    // Create a transaction
    let tx = TransactionRequest::default()
        .to(alice)
        .value(U256::from(1))
        .nonce(0)
        .gas_price(fees.max_fee_per_gas)
        .gas_limit(50_000);
    // Build and sign the transaction to get the envelope
    let tx_envelope = tx.build(&tester.l2_wallet).await?;
    // Encode the transaction
    let encoded = tx_envelope.encoded_2718();

    // Send using the sync method - this directly returns the receipt
    let receipt = tester
        .l2_provider
        .send_raw_transaction_sync(&encoded)
        .await?;
    assert!(receipt.status());

    // Verify receipt
    assert_eq!(receipt.to(), Some(alice));
    // The main idea that returned receipt should be already mined
    assert!(
        receipt.block_number().is_some(),
        "transaction should be mined"
    );
    assert_ne!(
        receipt.transaction_hash(),
        TxHash::ZERO,
        "should have valid tx hash"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn send_raw_transaction_sync_timeout() -> anyhow::Result<()> {
    // Test that the node returns an error when `eth_sendRawTransactionSync` timeouts
    let tester = Tester::builder().build().await?;

    let alice = tester.l2_wallet.default_signer().address();
    let fees = tester.l2_provider.estimate_eip1559_fees().await?;
    // Create a transaction
    let tx = TransactionRequest::default()
        .to(alice)
        .value(U256::from(1))
        // !!! NOTE !!! - nonce gap
        .nonce(1)
        .gas_price(fees.max_fee_per_gas)
        .gas_limit(50_000);
    // Build and sign the transaction to get the envelope
    let tx_envelope = tx.build(&tester.l2_wallet).await?;
    // Encode the transaction
    let encoded = tx_envelope.encoded_2718();

    // Send using the sync method - this directly returns the receipt
    let error = tester
        .l2_provider
        .send_raw_transaction_sync(&encoded)
        .await
        .expect_err("should fail");
    assert!(
        error
            .to_string()
            .contains("The transaction was added to the mempool but wasn't processed within")
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn estimate_gas_with_high_prices() -> anyhow::Result<()> {
    // Tests the estimations are accurate with high fee overrides.
    // Following config has high pubdata price, that makes base token transfer to take >21000 gas.
    let fee_config = FeeConfig {
        native_price_usd: 3e-9, // doesn't matter
        pubdata_price_override: Some(U128::from(10_000_000_000_000u64)),
        native_price_override: Some(U128::from(1_000_000u64)),
        base_fee_override: Some(U128::from(100_000_000u64)),
        native_per_gas: 100, // doesn't matter
        pubdata_price_cap: None,
    };
    let tester = Tester::builder()
        .fee_config(fee_config)
        .estimate_gas_pubdata_price_factor(1.0)
        .build()
        .await?;

    // Random address.
    let to = address!("0xa5d85D1D865F89a23A95d4F5F74850f289Dbc5f9");
    // Create a transaction
    let tx = TransactionRequest::default().to(to).value(U256::ONE);

    let gas = tester.l2_provider.estimate_gas(tx.clone()).await?;
    tracing::info!("Estimated gas: {gas}");

    let receipt = tester
        .l2_provider
        .send_transaction(tx)
        .await?
        .expect_successful_receipt()
        .await?;
    tracing::info!("Got receipt, gas used: {}", receipt.gas_used);

    Ok(())
}

#[test_log::test(tokio::test)]
async fn estimate_gas_without_balance() -> anyhow::Result<()> {
    // Test that the node can estimate transaction's gas even if sender does not have enough balance.
    let tester = Tester::setup().await?;
    let req = TransactionRequest::default()
        .to(address!("0xF8fF3e62E94807a5C687f418Fe36942dD3a24525"))
        .from(address!("0x38711eC715A5A32180427792Dc0e97f8E3303072"));
    let txs_requests = [
        // no gas price fields are specified
        req.clone(),
        // `gasPrice=0`
        req.clone().gas_price(0),
        // `maxPriorityFeePerGas=0`
        req.clone().max_priority_fee_per_gas(0),
        // `maxFeePerGas=0,maxPriorityFeePerGas=0`
        req.clone().max_fee_per_gas(0).max_priority_fee_per_gas(0),
    ];
    for (i, tx_request) in txs_requests.into_iter().enumerate() {
        let estimated_gas = tester.l2_provider.estimate_gas(tx_request).await?;
        tracing::info!("Estimated gas for tx #{i}: {estimated_gas}");
    }
    Ok(())
}

#[tracing::instrument(skip(provider))]
async fn wait_for_batch_commitment(
    diamond_proxy_address: Address,
    provider: &EthDynProvider,
    expected_batch_number: u64,
) -> B256 {
    let filter = Filter::new()
        .event_signature(BlockCommit::SIGNATURE_HASH)
        .address(diamond_proxy_address);

    loop {
        tracing::debug!("querying batch commitment logs");

        let logs = provider
            .get_logs(&filter)
            .await
            .expect("failed to get logs");
        for log in &logs {
            let topics = log.inner.data.topics();
            assert_eq!(topics.len(), 4);
            let batch_number = U256::from_be_bytes(topics[1].0);
            let batch_number =
                u64::try_from(batch_number).expect("incorrect batch number in event");
            if batch_number == expected_batch_number {
                tracing::info!(batch_number, "successfully waited for batch");
                return topics[2];
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tracing::instrument(skip(tester))]
async fn wait_for_proof(
    tester: &Tester,
    address: Address,
    storage_keys: &[B256],
    batch_number: u64,
) -> anyhow::Result<BatchStorageProof> {
    loop {
        let maybe_proof = tester
            .l2_zk_provider
            .get_storage_proof(address, storage_keys.to_vec(), batch_number)
            .await?;
        if let Some(proof) = maybe_proof {
            return Ok(proof);
        }
        tracing::info!("no proof yet, waiting");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[test_log::test(tokio::test)]
#[tracing::instrument]
async fn get_storage_proof() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;

    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;
    tracing::info!(?bridgehub_address);
    let chain_id = tester.l2_provider.get_chain_id().await?;
    tracing::info!(chain_id);

    // Get L1 state which contains diamond proxy address
    let l1_state = L1State::fetch(
        tester.l1_provider().clone().erased(),
        tester.l1_provider().clone().erased(),
        bridgehub_address,
        chain_id,
    )
    .await?;
    let diamond_proxy_address = l1_state.diamond_proxy_address_sl();
    tracing::info!(?diamond_proxy_address);

    let deploy_tx_receipt = Counter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");
    tracing::info!(?contract_address, "deployed counter");

    let queried_keys = [B256::repeat_byte(0), B256::repeat_byte(0x1f)];
    let batch_number = 2;
    let batch_commitment =
        wait_for_batch_commitment(diamond_proxy_address, tester.l1_provider(), batch_number).await;
    tracing::info!(?batch_commitment);

    let proof = wait_for_proof(&tester, contract_address, &queried_keys, batch_number).await?;
    tracing::info!(?proof, "got proof");
    let storage_view = proof
        .verify(contract_address, &queried_keys)
        .context("invalid proof")?;
    assert_eq!(storage_view.storage_commitment, batch_commitment);
    // The contract is not written to yet.
    assert_eq!(storage_view.storage_values, [None; 2]);

    tracing::info!("writing to counter contract");
    let counter = CounterInstance::new(contract_address, tester.l2_provider.clone());
    counter
        .increment(U256::from(42))
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    tracing::info!("written to counter");

    let new_batch_number = 3;
    let new_batch_commitment = wait_for_batch_commitment(
        diamond_proxy_address,
        tester.l1_provider(),
        new_batch_number,
    )
    .await;
    assert_ne!(new_batch_commitment, batch_commitment);

    let proof = wait_for_proof(&tester, contract_address, &queried_keys, new_batch_number).await?;
    tracing::info!(?proof, "got proof");
    let storage_view = proof
        .verify(contract_address, &queried_keys)
        .context("invalid proof")?;
    assert_eq!(storage_view.storage_commitment, new_batch_commitment);
    assert_eq!(
        storage_view.storage_values,
        [Some(B256::left_padding_from(&[42])), None]
    );

    Ok(())
}
