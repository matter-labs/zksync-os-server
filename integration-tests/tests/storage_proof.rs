use std::time::Duration;

use alloy::network::ReceiptResponse;
use alloy::primitives::{B256, U256};
use alloy::providers::Provider;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::Counter;
use zksync_os_integration_tests::contracts::Counter::CounterInstance;
use zksync_os_integration_tests::provider::ZksyncApi;
use zksync_os_verify_storage_proof::l1::{fetch_stored_batch_hash, resolve_diamond_proxy};
use zksync_os_verify_storage_proof::{VerifyParams, verify_storage_proof};

/// Waits until `storedBatchHash(batch_number)` returns a non-zero value on L1.
async fn wait_for_batch_commitment(tester: &Tester, batch_number: u64) {
    let bridgehub_address = tester
        .l2_zk_provider
        .get_bridgehub_contract()
        .await
        .unwrap();
    let diamond_proxy = resolve_diamond_proxy(
        tester.l1_provider(),
        &tester.l2_zk_provider,
        None,
        Some(bridgehub_address),
    )
    .await
    .unwrap();

    loop {
        match fetch_stored_batch_hash(tester.l1_provider(), diamond_proxy, batch_number).await {
            Ok(_) => return,
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

#[test_log::test(tokio::test)]
async fn verify_storage_proof_with_l1_contract() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;

    let chain_id = tester.l2_provider.get_chain_id().await?;
    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;
    let l1_state = L1State::fetch(
        tester.l1_provider().clone().erased(),
        tester.l1_provider().clone().erased(),
        bridgehub_address,
        chain_id,
    )
    .await?;
    let diamond_proxy_address = l1_state.diamond_proxy_address_sl();

    // Deploy a counter contract and write to it
    let deploy_tx_receipt = Counter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");

    let counter = CounterInstance::new(contract_address, tester.l2_provider.clone());
    counter
        .increment(U256::from(42))
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    // Wait for batch 2 to be committed on L1
    let batch_number = 2;
    wait_for_batch_commitment(&tester, batch_number).await;

    // Wait for proof to be available
    let queried_keys = vec![B256::ZERO];
    loop {
        let maybe_proof = tester
            .l2_zk_provider
            .get_storage_proof(contract_address, queried_keys.clone(), batch_number)
            .await?;
        if maybe_proof.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Run the full verification pipeline using our library with explicit diamond proxy
    let result = verify_storage_proof(
        tester.l1_provider(),
        &tester.l2_zk_provider,
        VerifyParams {
            address: contract_address,
            keys: queried_keys,
            batch_number,
            l1_contract: Some(diamond_proxy_address),
            bridgehub: None,
        },
    )
    .await?;

    // The reconstructed hash should match L1
    assert_eq!(result.computed_batch_hash, result.on_chain_batch_hash);
    // Slot 0 should have value 42 (counter was incremented)
    assert_eq!(
        result.storage_values[0],
        (B256::ZERO, Some(B256::left_padding_from(&[42])))
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn verify_storage_proof_with_bridgehub_discovery() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;

    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;

    // Deploy a counter contract
    let deploy_tx_receipt = Counter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");

    // Wait for batch 2 to be committed on L1
    let batch_number = 2;
    wait_for_batch_commitment(&tester, batch_number).await;

    // Wait for proof to be available
    let queried_keys = vec![B256::ZERO];
    loop {
        let maybe_proof = tester
            .l2_zk_provider
            .get_storage_proof(contract_address, queried_keys.clone(), batch_number)
            .await?;
        if maybe_proof.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Run the full verification pipeline with bridgehub auto-discovery
    let result = verify_storage_proof(
        tester.l1_provider(),
        &tester.l2_zk_provider,
        VerifyParams {
            address: contract_address,
            keys: queried_keys,
            batch_number,
            l1_contract: None,
            bridgehub: Some(bridgehub_address),
        },
    )
    .await?;

    assert_eq!(result.computed_batch_hash, result.on_chain_batch_hash);
    // Contract was just deployed, slot 0 should be empty (counter not incremented)
    assert_eq!(result.storage_values[0], (B256::ZERO, None));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn verify_storage_proof_empty_slot() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;

    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let l1_state = L1State::fetch(
        tester.l1_provider().clone().erased(),
        tester.l1_provider().clone().erased(),
        bridgehub_address,
        chain_id,
    )
    .await?;
    let diamond_proxy_address = l1_state.diamond_proxy_address_sl();

    // Deploy a counter contract but don't write to it
    let deploy_tx_receipt = Counter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");

    let batch_number = 2;
    wait_for_batch_commitment(&tester, batch_number).await;

    let queried_keys = vec![B256::ZERO, B256::repeat_byte(0x1f)];
    loop {
        let maybe_proof = tester
            .l2_zk_provider
            .get_storage_proof(contract_address, queried_keys.clone(), batch_number)
            .await?;
        if maybe_proof.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let result = verify_storage_proof(
        tester.l1_provider(),
        &tester.l2_zk_provider,
        VerifyParams {
            address: contract_address,
            keys: queried_keys.clone(),
            batch_number,
            l1_contract: Some(diamond_proxy_address),
            bridgehub: None,
        },
    )
    .await?;

    assert_eq!(result.computed_batch_hash, result.on_chain_batch_hash);
    // Both slots should be empty
    for (key, value) in &result.storage_values {
        assert!(value.is_none(), "Expected empty slot for key {key}");
    }

    Ok(())
}
