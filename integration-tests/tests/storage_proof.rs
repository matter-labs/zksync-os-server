use std::time::Duration;

use alloy::primitives::{B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use zksync_os_contract_interface::IExecutor::BlockCommit;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::Counter::CounterInstance;
use zksync_os_integration_tests::contracts::Counter;
use zksync_os_integration_tests::provider::ZksyncApi;
use zksync_os_verify_storage_proof::{VerifyParams, verify_storage_proof};

async fn wait_for_batch_commitment(tester: &Tester, batch_number: u64) -> B256 {
    let chain_id = tester.l2_provider.get_chain_id().await.unwrap();
    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await.unwrap();
    let l1_state = L1State::fetch(
        tester.l1_provider().clone().erased(),
        tester.l1_provider().clone().erased(),
        bridgehub_address,
        chain_id,
    )
    .await
    .unwrap();
    let diamond_proxy_address = l1_state.diamond_proxy_address_sl();

    let filter = Filter::new()
        .event_signature(BlockCommit::SIGNATURE_HASH)
        .address(diamond_proxy_address);

    loop {
        let logs = tester
            .l1_provider()
            .get_logs(&filter)
            .await
            .expect("failed to get logs");
        for log in &logs {
            let topics = log.inner.data.topics();
            let bn = U256::from_be_bytes(topics[1].0);
            if u64::try_from(bn).unwrap() == batch_number {
                return topics[2];
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
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
    let _batch_commitment = wait_for_batch_commitment(&tester, batch_number).await;

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

    // The commitment should match L1
    assert_eq!(result.storage_commitment, result.l1_batch_hash);
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
    let _batch_commitment = wait_for_batch_commitment(&tester, batch_number).await;

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

    assert_eq!(result.storage_commitment, result.l1_batch_hash);
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
    let _batch_commitment = wait_for_batch_commitment(&tester, batch_number).await;

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

    assert_eq!(result.storage_commitment, result.l1_batch_hash);
    // Both slots should be empty
    for (key, value) in &result.storage_values {
        assert!(value.is_none(), "Expected empty slot for key {key}");
    }

    Ok(())
}
