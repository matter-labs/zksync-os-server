use alloy::network::{ReceiptResponse, TxSigner};
use alloy::providers::Provider;
use regex::Regex;
use std::time::Duration;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
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
