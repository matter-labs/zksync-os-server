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
use zksync_os_merkle_tree_api::flat::InnerStorageSlotProof;
use zksync_os_rpc_api::types::BatchStorageProof;
use zksync_os_verify_storage_proof::l1::{fetch_stored_batch_hash, resolve_diamond_proxy};
use zksync_os_verify_storage_proof::{VerificationResult, VerifyParams, verify_storage_proof};

fn log_proof(proof: &BatchStorageProof) {
    tracing::info!(address = %proof.address, "=== Storage Proof ===");

    let sc = &proof.state_commitment_preimage;
    tracing::info!(
        next_free_slot = %sc.next_free_slot,
        block_number = %sc.block_number,
        last_block_timestamp = %sc.last_block_timestamp,
        last_256_block_hashes_blake = %sc.last_256_block_hashes_blake,
        "state commitment preimage"
    );

    let l1 = &proof.l1_verification_data;
    tracing::info!(
        batch_number = l1.batch_number,
        number_of_layer1_txs = l1.number_of_layer1_txs,
        priority_operations_hash = %l1.priority_operations_hash,
        l2_to_l1_logs_root_hash = %l1.l2_to_l1_logs_root_hash,
        commitment = %l1.commitment,
        "L1 verification data"
    );

    for (i, slot_proof) in proof.storage_proofs.iter().enumerate() {
        match &slot_proof.proof {
            InnerStorageSlotProof::Existing(entry) => {
                tracing::info!(
                    slot = i,
                    key = %slot_proof.key.0,
                    index = entry.index,
                    value = %entry.value,
                    next_index = entry.next_index,
                    siblings = entry.siblings.len(),
                    "slot proof (existing)"
                );
            }
            InnerStorageSlotProof::NonExisting {
                left_neighbor,
                right_neighbor,
            } => {
                tracing::info!(
                    slot = i,
                    key = %slot_proof.key.0,
                    left_key = %left_neighbor.leaf_key,
                    left_index = left_neighbor.inner.index,
                    right_key = %right_neighbor.leaf_key,
                    right_index = right_neighbor.inner.index,
                    "slot proof (non-existing)"
                );
            }
        }
    }
}

fn log_result(result: &VerificationResult) {
    let hashes_match = result.computed_batch_hash == result.on_chain_batch_hash;
    tracing::info!(
        computed = %result.computed_batch_hash,
        on_chain = %result.on_chain_batch_hash,
        hashes_match,
        "=== Verification Result ==="
    );
    for (key, value) in &result.storage_values {
        match value {
            Some(v) => tracing::info!(key = %key, value = %v, "storage slot"),
            None => tracing::info!(key = %key, "storage slot (empty)"),
        }
    }
}

/// Waits until `storedBatchHash(batch_number)` returns a non-zero value on L1.
async fn wait_for_batch_commitment(tester: &Tester, batch_number: u64) {
    tracing::info!(batch_number, "waiting for batch commitment on L1");
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
            Ok(hash) => {
                tracing::info!(batch_number, ?hash, "batch committed on L1");
                return;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

#[test_log::test(tokio::test)]
async fn verify_storage_proof_with_l1_contract() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;

    let chain_id = tester.l2_provider.get_chain_id().await?;
    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;
    tracing::info!(?bridgehub_address, chain_id, "fetched L1 state");
    let l1_state = L1State::fetch(
        tester.l1_provider().clone().erased(),
        tester.l1_provider().clone().erased(),
        bridgehub_address,
        chain_id,
    )
    .await?;
    let diamond_proxy_address = l1_state.diamond_proxy_address_sl();
    tracing::info!(?diamond_proxy_address, "resolved diamond proxy");

    // Deploy a counter contract and write to it
    let deploy_tx_receipt = Counter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");
    tracing::info!(?contract_address, "deployed counter contract");

    let counter = CounterInstance::new(contract_address, tester.l2_provider.clone());
    counter
        .increment(U256::from(42))
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    tracing::info!("incremented counter to 42");

    // Wait for batch 2 to be committed on L1
    let batch_number = 2;
    wait_for_batch_commitment(&tester, batch_number).await;

    // Wait for proof to be available
    tracing::info!(
        batch_number,
        ?contract_address,
        "waiting for proof availability"
    );
    let queried_keys = vec![B256::ZERO];
    let proof = loop {
        if let Some(proof) = tester
            .l2_zk_provider
            .get_storage_proof(contract_address, queried_keys.clone(), batch_number)
            .await?
        {
            break proof;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    log_proof(&proof);

    // Run the full verification pipeline using our library with explicit diamond proxy
    tracing::info!("running verification with explicit diamond proxy");
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
    log_result(&result);

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
    tracing::info!(?bridgehub_address, "using bridgehub auto-discovery");

    // Deploy a counter contract
    let deploy_tx_receipt = Counter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");
    tracing::info!(?contract_address, "deployed counter contract");

    // Wait for batch 2 to be committed on L1
    let batch_number = 2;
    wait_for_batch_commitment(&tester, batch_number).await;

    // Wait for proof to be available
    tracing::info!(
        batch_number,
        ?contract_address,
        "waiting for proof availability"
    );
    let queried_keys = vec![B256::ZERO];
    let proof = loop {
        if let Some(proof) = tester
            .l2_zk_provider
            .get_storage_proof(contract_address, queried_keys.clone(), batch_number)
            .await?
        {
            break proof;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    log_proof(&proof);

    // Run the full verification pipeline with bridgehub auto-discovery
    tracing::info!("running verification with bridgehub discovery");
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
    log_result(&result);

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
    tracing::info!(?bridgehub_address, chain_id, "fetched L1 state");
    let l1_state = L1State::fetch(
        tester.l1_provider().clone().erased(),
        tester.l1_provider().clone().erased(),
        bridgehub_address,
        chain_id,
    )
    .await?;
    let diamond_proxy_address = l1_state.diamond_proxy_address_sl();
    tracing::info!(?diamond_proxy_address, "resolved diamond proxy");

    // Deploy a counter contract but don't write to it
    let deploy_tx_receipt = Counter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");
    tracing::info!(?contract_address, "deployed counter contract (no writes)");

    let batch_number = 2;
    wait_for_batch_commitment(&tester, batch_number).await;

    tracing::info!(
        batch_number,
        ?contract_address,
        "waiting for proof availability"
    );
    let queried_keys = vec![B256::ZERO, B256::repeat_byte(0x1f)];
    let proof = loop {
        if let Some(proof) = tester
            .l2_zk_provider
            .get_storage_proof(contract_address, queried_keys.clone(), batch_number)
            .await?
        {
            break proof;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    log_proof(&proof);

    tracing::info!("running verification for empty slots");
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
    log_result(&result);

    assert_eq!(result.computed_batch_hash, result.on_chain_batch_hash);
    // Both slots should be empty
    for (key, value) in &result.storage_values {
        assert!(value.is_none(), "Expected empty slot for key {key}");
    }

    Ok(())
}
