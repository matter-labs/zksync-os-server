use alloy::consensus::Transaction as _;
use alloy::network::{TransactionBuilder, TransactionResponse};
use alloy::primitives::{Address, TxHash, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Transaction, TransactionRequest};
use zksync_os_integration_tests::Tester;
use zksync_os_types::L2Envelope;

#[test_log::test(tokio::test)]
async fn new_block_filter() -> anyhow::Result<()> {
    // Test that `eth_newBlockFilter` works as expected
    let tester = Tester::setup().await?;

    let filter_id = tester.l2_provider.new_block_filter().await?;

    let receipt = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(100)),
        )
        .await?
        .get_receipt()
        .await?;

    let new_blocks = tester
        .l2_provider
        .get_filter_changes::<B256>(filter_id)
        .await?;
    assert!(
        new_blocks.contains(&receipt.block_hash.unwrap()),
        "new filter blocks do not contain receipt's block hash"
    );

    let new_blocks = tester
        .l2_provider
        .get_filter_changes::<B256>(filter_id)
        .await?;
    assert!(
        new_blocks.is_empty(),
        "filter should not pick up any new blocks"
    );

    tester.l2_provider.uninstall_filter(filter_id).await?;
    let err = tester
        .l2_provider
        .get_filter_changes::<B256>(filter_id)
        .await
        .expect_err("filter should be uninstalled");
    assert!(err.to_string().contains("filter not found"));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn pending_tx_hash_filter() -> anyhow::Result<()> {
    // Test that `eth_newPendingTransactionFilter(full=false)` works as expected
    let tester = Tester::setup().await?;

    let filter_id = tester
        .l2_provider
        .new_pending_transactions_filter(false)
        .await?;

    let pending_tx = tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(100)),
        )
        .await?;
    let pending_tx_hash = *pending_tx.tx_hash();

    let new_tx_hashes = tester
        .l2_provider
        .get_filter_changes::<TxHash>(filter_id)
        .await?;
    assert!(
        new_tx_hashes.contains(&pending_tx_hash),
        "filter's pending txs do not contain our pending tx"
    );

    let new_tx_hashes = tester
        .l2_provider
        .get_filter_changes::<TxHash>(filter_id)
        .await?;
    assert!(
        new_tx_hashes.is_empty(),
        "filter should not pick up any new pending transactions"
    );

    tester.l2_provider.uninstall_filter(filter_id).await?;
    let err = tester
        .l2_provider
        .get_filter_changes::<TxHash>(filter_id)
        .await
        .expect_err("filter should be uninstalled");
    assert!(err.to_string().contains("filter not found"));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn pending_tx_full_filter() -> anyhow::Result<()> {
    // Test that `eth_newPendingTransactionFilter(full=true)` works as expected
    let tester = Tester::setup().await?;

    let filter_id = tester
        .l2_provider
        .new_pending_transactions_filter(true)
        .await?;

    let to = Address::random();
    let value = U256::from(100);
    let pending_tx = tester
        .l2_provider
        .send_transaction(TransactionRequest::default().with_to(to).with_value(value))
        .await?;
    let pending_tx_hash = *pending_tx.tx_hash();

    let new_txs = tester
        .l2_provider
        .get_filter_changes::<Transaction<L2Envelope>>(filter_id)
        .await?;
    let new_tx = new_txs
        .into_iter()
        .find(|tx| tx.tx_hash() == pending_tx_hash)
        .expect("filter's pending txs do not contain our pending tx");
    assert_eq!(new_tx.to(), Some(to), "pending tx's `to` does not match");
    assert_eq!(new_tx.value(), value, "pending tx's `value` does not match");

    let new_txs = tester
        .l2_provider
        .get_filter_changes::<Transaction<L2Envelope>>(filter_id)
        .await?;
    assert!(
        new_txs.is_empty(),
        "filter should not pick up any new pending transactions"
    );

    tester.l2_provider.uninstall_filter(filter_id).await?;
    let err = tester
        .l2_provider
        .get_filter_changes::<Transaction<L2Envelope>>(filter_id)
        .await
        .expect_err("filter should be uninstalled");
    assert!(err.to_string().contains("filter not found"));

    Ok(())
}
