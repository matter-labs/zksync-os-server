use alloy::eips::BlockNumberOrTag;
use alloy::network::{ReceiptResponse, TransactionBuilder};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use zksync_os_alloy_ext::provider::ZksyncApi;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};
use zksync_os_rpc_api::finality::FinalityStage;

/// Submits a transaction, waits for it to be executed on L1, and checks that every `zks` finality
/// method reports a consistent view of the block / transaction / batch and the node-wide frontiers.
#[test_multisetup([CURRENT_TO_L1])]
async fn finality_progression(tester: Tester) -> anyhow::Result<()> {
    // The `zks_*` finality methods live on the ZKsync-network provider; transactions go through the
    // Ethereum-compatible one.
    let zk = &tester.l2_zk_provider;

    // 10x buffer on gas price so the tx doesn't get stuck while L1 catches up.
    let gas_price = tester.l2_provider.get_gas_price().await? * 10;
    let tx = TransactionRequest::default()
        .with_to(Address::random())
        .with_value(U256::from(100))
        .with_gas_price(gas_price);
    let receipt = tester
        .l2_provider
        .send_transaction(tx)
        .await?
        .expect_to_execute()
        .await?;
    let block_number = receipt.block_number().expect("receipt has a block number");
    let tx_hash = receipt.transaction_hash();

    // Node-wide snapshot: ordering invariant + liveness for the executed block.
    let status = zk.get_finality_status().await?;
    assert!(status.last_finalized_executed_block <= status.last_executed_block);
    assert!(status.last_executed_block <= status.last_committed_block);
    assert!(status.last_committed_block <= status.last_sealed_block);
    assert!(status.last_finalized_executed_batch <= status.last_executed_batch);
    assert!(status.last_executed_batch <= status.last_committed_batch);
    assert!(status.last_executed_block >= block_number);

    // Block finality: at least executed, with the containing batch resolved.
    let block_finality = zk
        .get_block_finality(BlockNumberOrTag::Number(block_number))
        .await?
        .expect("a known block has a finality status");
    assert!(matches!(
        block_finality.stage,
        FinalityStage::Executed | FinalityStage::Finalized
    ));
    assert_eq!(block_finality.block_number, Some(block_number));
    let batch_number = block_finality
        .batch_number
        .expect("an executed block belongs to a batch");

    // Transaction finality matches its including block.
    let tx_finality = zk
        .get_transaction_finality(tx_hash)
        .await?
        .expect("a known transaction has a finality status");
    assert!(matches!(
        tx_finality.stage,
        FinalityStage::Executed | FinalityStage::Finalized
    ));
    assert_eq!(tx_finality.block_number, Some(block_number));
    assert_eq!(tx_finality.batch_number, Some(batch_number));

    // Batch finality: the block's batch is at least executed.
    let batch_finality = zk.get_batch_finality(batch_number).await?;
    assert!(matches!(
        batch_finality.stage,
        FinalityStage::Executed | FinalityStage::Finalized
    ));
    assert_eq!(batch_finality.batch_number, Some(batch_number));

    // Tag self-consistency: the `finalized` block is Finalized; the `safe` block is at least Committed.
    let finalized = zk
        .get_block_finality(BlockNumberOrTag::Finalized)
        .await?
        .expect("the finalized block has a finality status");
    assert!(matches!(finalized.stage, FinalityStage::Finalized));
    let safe = zk
        .get_block_finality(BlockNumberOrTag::Safe)
        .await?
        .expect("the safe block has a finality status");
    assert!(matches!(
        safe.stage,
        FinalityStage::Committed | FinalityStage::Executed | FinalityStage::Finalized
    ));

    // Unknown / future inputs.
    let future_block = status.last_sealed_block + 1_000_000;
    assert!(
        zk.get_block_finality(BlockNumberOrTag::Number(future_block))
            .await?
            .is_none()
    );
    let future_batch = status.last_committed_batch + 1_000_000;
    let future_batch_finality = zk.get_batch_finality(future_batch).await?;
    assert!(matches!(
        future_batch_finality.stage,
        FinalityStage::Pending
    ));
    assert!(zk.get_transaction_finality(B256::random()).await?.is_none());

    Ok(())
}
