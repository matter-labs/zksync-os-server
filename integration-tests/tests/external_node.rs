use std::time::Duration;

use alloy::eips::BlockId;
use alloy::network::TxSigner;
use alloy::primitives::U256;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::{network::ReceiptResponse, primitives::Address};
use backon::{ConstantBuilder, Retryable};
use zksync_os_integration_tests::{Tester, assert_traits::ReceiptAssert, contracts::EventEmitter};

#[test_log::test(tokio::test)]
async fn transaction_replay() -> anyhow::Result<()> {
    let main_node = Tester::setup().await?;
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
#[test_log::test(tokio::test)]
async fn does_not_get_stuck() -> anyhow::Result<()> {
    let main_node = Tester::setup().await?;
    let en1 = main_node.launch_external_node().await?;

    let (send, mut recv) = tokio::sync::mpsc::channel(100);

    const REPEATS: usize = 200;

    tokio::spawn(async move {
        for _ in 0..REPEATS {
            let deploy_tx_receipt = EventEmitter::deploy_builder(main_node.l2_provider.clone())
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
            .with_delay(Duration::from_secs(1))
            .with_max_times(10),
    )
    .await
}

#[test_log::test(tokio::test)]
async fn forward_transactions() -> anyhow::Result<()> {
    let main_node = Tester::setup().await?;
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
