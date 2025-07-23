use alloy::network::ReceiptResponse;
use alloy::providers::Provider;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::contracts::EventEmitter;

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
        .get_receipt()
        .await?;
    assert!(deploy_tx_receipt.status(), "deploy transaction failed");
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
