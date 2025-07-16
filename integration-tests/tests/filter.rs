use alloy::consensus::Transaction as _;
use alloy::network::{TransactionBuilder, TransactionResponse};
use alloy::primitives::{Address, TxHash, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log, Transaction, TransactionRequest};
use alloy::sol_types::SolEvent;
use zksync_os_integration_tests::contracts::EventEmitter;
use zksync_os_integration_tests::contracts::EventEmitter::TestEvent;
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

#[test_log::test(tokio::test)]
async fn new_log_filter() -> anyhow::Result<()> {
    // Test that `eth_newFilter` works as expected
    let tester = Tester::setup().await?;

    let event_emitter = EventEmitter::deploy(&tester.l2_provider).await?;
    let filter = Filter::new()
        .address(*event_emitter.address())
        .event_signature(TestEvent::SIGNATURE_HASH);
    let filter_id = tester.l2_provider.new_filter(&filter).await?;

    let receipt = event_emitter
        .emitEvent(U256::from(42))
        .send()
        .await?
        .get_receipt()
        .await?;
    assert!(receipt.status(), "transaction failed");
    let block = tester
        .l2_provider
        .get_block_by_number(receipt.block_number.unwrap().into())
        .await?
        .expect("no block found");

    let mut new_logs = tester
        .l2_provider
        .get_filter_changes::<Log>(filter_id)
        .await?;
    assert_eq!(new_logs.len(), 1, "filter should pick up one new log");
    let new_log = new_logs.swap_remove(0);
    assert_eq!(
        new_log.block_hash, receipt.block_hash,
        "log `block_hash` does not match"
    );
    assert_eq!(
        new_log.block_number, receipt.block_number,
        "log `block_number` does not match"
    );
    assert_eq!(
        new_log.block_timestamp,
        Some(block.header.timestamp),
        "log `block_timestamp` does not match"
    );
    assert_eq!(
        new_log.transaction_hash,
        Some(receipt.transaction_hash),
        "log `transaction_hash` does not match"
    );
    assert_eq!(
        new_log.transaction_index, receipt.transaction_index,
        "log `transaction_index` does not match"
    );
    assert_eq!(new_log.log_index, Some(0), "log `log_index` does not match");
    assert!(!new_log.removed, "log is removed");

    let new_logs = tester
        .l2_provider
        .get_filter_changes::<Log>(filter_id)
        .await?;
    assert!(
        new_logs.is_empty(),
        "filter should not pick up any new logs"
    );

    tester.l2_provider.uninstall_filter(filter_id).await?;
    let err = tester
        .l2_provider
        .get_filter_changes::<Log>(filter_id)
        .await
        .expect_err("filter should be uninstalled");
    assert!(err.to_string().contains("filter not found"));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn get_filter_logs() -> anyhow::Result<()> {
    // Test that `eth_getFilterLogs` works as expected
    let tester = Tester::setup().await?;

    let event_emitter = EventEmitter::deploy(&tester.l2_provider).await?;
    let filter = Filter::new()
        .address(*event_emitter.address())
        .event_signature(TestEvent::SIGNATURE_HASH);
    let filter_id = tester.l2_provider.new_filter(&filter).await?;

    let receipt = event_emitter
        .emitEvent(U256::from(42))
        .send()
        .await?
        .get_receipt()
        .await?;
    assert!(receipt.status(), "transaction failed");
    let block = tester
        .l2_provider
        .get_block_by_number(receipt.block_number.unwrap().into())
        .await?
        .expect("no block found");

    let mut logs = tester.l2_provider.get_filter_logs(filter_id).await?;
    assert_eq!(logs.len(), 1, "filter should pick up one log");
    let log = logs.swap_remove(0);
    assert_eq!(
        log.block_hash, receipt.block_hash,
        "log `block_hash` does not match"
    );
    assert_eq!(
        log.block_number, receipt.block_number,
        "log `block_number` does not match"
    );
    assert_eq!(
        log.block_timestamp,
        Some(block.header.timestamp),
        "log `block_timestamp` does not match"
    );
    assert_eq!(
        log.transaction_hash,
        Some(receipt.transaction_hash),
        "log `transaction_hash` does not match"
    );
    assert_eq!(
        log.transaction_index, receipt.transaction_index,
        "log `transaction_index` does not match"
    );
    assert_eq!(log.log_index, Some(0), "log `log_index` does not match");
    assert!(!log.removed, "log is removed");

    let logs = tester.l2_provider.get_filter_logs(filter_id).await?;
    assert_eq!(logs.len(), 1, "filter should pick up same log again");

    tester.l2_provider.uninstall_filter(filter_id).await?;
    let err = tester
        .l2_provider
        .get_filter_changes::<Log>(filter_id)
        .await
        .expect_err("filter should be uninstalled");
    assert!(err.to_string().contains("filter not found"));

    Ok(())
}
