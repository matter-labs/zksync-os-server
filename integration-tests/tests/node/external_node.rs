use std::time::Duration;

use alloy::eips::BlockId;
use alloy::network::TxSigner;
use alloy::primitives::U256;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::{network::ReceiptResponse, primitives::Address};
use backon::{ConstantBuilder, Retryable};
use zksync_os_integration_tests::BATCH_VERIFICATION_KEYS;
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
use zksync_os_integration_tests::{
    CURRENT_TO_L1, NEXT_TO_GATEWAY, Tester, TesterBuilder, assert_traits::ReceiptAssert,
    contracts::EventEmitter, test_multisetup,
};
use zksync_os_server::config::Config;

#[test_multisetup([CURRENT_TO_L1, NEXT_TO_GATEWAY])]
async fn batch_verification_works(builder: TesterBuilder) -> anyhow::Result<()> {
    let builder = builder.batch_verification(1);
    let main_node = builder.build().await?;

    let _en1 = main_node
        .launch_external_node_overrides(|config: &mut Config| {
            let bv_config = &mut config.batch_verification_config;
            bv_config.client_enabled = true;
        })
        .await?;

    let deploy_tx_receipt = EventEmitter::deploy_builder(main_node.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    // Check batch is eventually finalized
    main_node
        .l2_zk_provider
        .wait_finalized_with_timeout(
            deploy_tx_receipt.block_number.unwrap(),
            zksync_os_integration_tests::assert_traits::DEFAULT_TIMEOUT,
        )
        .await?;
    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn batch_verification_without_enough_ens(builder: TesterBuilder) -> anyhow::Result<()> {
    let builder = builder.batch_verification(2);
    let main_node = builder.build().await?;

    let _en1 = main_node
        .launch_external_node_overrides(|config: &mut Config| {
            let bv_config = &mut config.batch_verification_config;
            bv_config.client_enabled = true;
        })
        .await?;

    // Do some random transaction
    let _deploy_tx_receipt = EventEmitter::deploy_builder(main_node.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    // First block should not get finalized because EN with 2FA is needed.
    // Use a shorter timeout: if finalization hasn't happened in 20s, it won't.
    main_node
        .l2_zk_provider
        .wait_not_finalized(1, Duration::from_secs(20))
        .await?;
    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
async fn batch_verification_with_2_ens(builder: TesterBuilder) -> anyhow::Result<()> {
    let builder = builder.batch_verification(2);
    let main_node = builder.build().await?;

    let _en1 = main_node
        .launch_external_node_overrides(|config: &mut Config| {
            let bv_config = &mut config.batch_verification_config;
            bv_config.client_enabled = true;
            bv_config.signing_key = BATCH_VERIFICATION_KEYS[0].into();
        })
        .await?;

    // First block should not get finalized because 2 EN with 2FA are needed.
    // Use a shorter timeout: if finalization hasn't happened in 20s, it won't.
    main_node
        .l2_zk_provider
        .wait_not_finalized(1, Duration::from_secs(20))
        .await?;

    let _en2 = main_node
        .launch_external_node_overrides(|config: &mut Config| {
            let bv_config = &mut config.batch_verification_config;
            bv_config.client_enabled = true;
            bv_config.signing_key = BATCH_VERIFICATION_KEYS[1].into();
        })
        .await?;

    // Do some random transaction
    let deploy_tx_receipt = EventEmitter::deploy_builder(main_node.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    // Should finalize everything because we have enough ENs now
    main_node
        .l2_zk_provider
        .wait_finalized_with_timeout(
            deploy_tx_receipt.block_number.unwrap(),
            zksync_os_integration_tests::assert_traits::DEFAULT_TIMEOUT,
        )
        .await?;
    Ok(())
}

#[test_multisetup([CURRENT_TO_L1, NEXT_TO_GATEWAY])]
#[test_builder(|builder| builder.enable_p2p())]
async fn transaction_replay(main_node: Tester) -> anyhow::Result<()> {
    let en1 = main_node.launch_external_node().await?;

    let deploy_tx_receipt = EventEmitter::deploy_builder(main_node.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");

    check_contract_present(&en1, contract_address).await?;

    let en2 = main_node.launch_external_node().await?;

    check_contract_present(&en2, contract_address).await?;

    let deploy_tx_receipt = EventEmitter::deploy_builder(main_node.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    let contract_address = deploy_tx_receipt
        .contract_address()
        .expect("no contract deployed");

    check_contract_present(&en1, contract_address).await?;
    check_contract_present(&en2, contract_address).await?;

    Ok(())
}

/// It is easy to write to a channel that the EN doesn't need
/// which leads to the EN getting stuck when the channel is full.
#[test_multisetup([CURRENT_TO_L1])]
#[test_builder(|builder| builder.enable_p2p())]
#[test_runtime(flavor = "multi_thread")]
async fn does_not_get_stuck(main_node: Tester) -> anyhow::Result<()> {
    let en1 = main_node.launch_external_node().await?;

    let (send, mut recv) = tokio::sync::mpsc::channel(100);

    // 30 deployments is sufficient to fill channel buffers and detect deadlocks,
    // while avoiding the excessive runtime of the previous 200 iterations.
    const REPEATS: usize = 30;

    let main_node_provider = main_node.l2_provider.clone();
    tokio::spawn(async move {
        for _ in 0..REPEATS {
            let deploy_tx_receipt = EventEmitter::deploy_builder(&main_node_provider)
                .send()
                .await
                .unwrap()
                .expect_successful_receipt()
                .await
                .unwrap();

            let contract_address = deploy_tx_receipt
                .contract_address()
                .expect("no contract deployed");

            send.send(contract_address).await.unwrap();
        }
    });

    for _ in 0..REPEATS {
        let contract_address = recv.recv().await.unwrap();
        check_contract_present(&en1, contract_address).await?;
    }

    // Make sure we hold `main_node` until the end of the test
    drop(main_node);

    Ok(())
}

async fn check_contract_present(en: &Tester, contract_address: Address) -> anyhow::Result<()> {
    (|| async {
        let latest_code = en.l2_provider.get_code_at(contract_address).await?;
        if latest_code == EventEmitter::DEPLOYED_BYTECODE {
            Ok(())
        } else {
            Err(anyhow::anyhow!("deployed bytecode mismatch"))
        }
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_millis(200))
            .with_max_times(50),
    )
    .await
}

#[test_multisetup([CURRENT_TO_L1])]
#[test_builder(|builder| builder.enable_p2p())]
async fn forward_transactions(main_node: Tester) -> anyhow::Result<()> {
    let en = main_node.launch_external_node().await?;
    let alice = en.l2_wallet.default_signer().address();

    // Alice's initial nonce
    let alice_nonce_before = en.l2_provider.get_transaction_count(alice).await?;

    // Submit transaction to EN; we expect that transaction will be forwarded to the main node
    let pending_tx = en
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .value(U256::from(1))
                .to(Address::random()),
        )
        .await?
        .register()
        .await?;

    // Alice's **pending** nonce after transaction was submitted
    let alice_nonce_mn_after = main_node
        .l2_provider
        .get_transaction_count(alice)
        .block_id(BlockId::pending())
        .await?;
    let alice_nonce_en_after = en
        .l2_provider
        .get_transaction_count(alice)
        .block_id(BlockId::pending())
        .await?;

    // Main node is aware of the transaction because EN forwarded it
    assert_eq!(alice_nonce_mn_after, alice_nonce_before + 1);
    // External node is aware of the transaction because it was saved to mempool
    assert_eq!(alice_nonce_en_after, alice_nonce_before + 1);

    // Wait for tx to finalize and validate that both main and external nodes have identical receipt
    let tx_hash = pending_tx.await?;
    let mn_receipt = main_node
        .l2_provider
        .get_transaction_receipt(tx_hash)
        .await?;
    let en_receipt = en.l2_provider.get_transaction_receipt(tx_hash).await?;
    assert_eq!(mn_receipt, en_receipt);

    Ok(())
}
