use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::LocalSigner;
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};
use zksync_os_server::config::RebuildBlocksConfig;

const BLOCKS_TO_MINE_BEFORE_REBUILD: u64 = 10;
const BLOCKS_FROM_TIP_TO_EMPTY: u64 = 4;
const TRANSACTION_SEND_INTERVAL: Duration = Duration::from_millis(5);

#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn rebuild_after_emptying_historical_block_preserves_unrelated_l2_txs() -> anyhow::Result<()>
{
    let tester = Tester::setup_with_overrides(|config| {
        config.batcher_config.enabled = false;
        config.sequencer_config.block_time = Duration::from_millis(50);
    })
    .await?;

    // This test empties an older block from the main sender, which makes that sender's later
    // transactions invalid because their nonces become too high. A second sender contributes the
    // last historical block so we can assert rebuild still reaches the tip and preserves
    // unrelated L2 transactions.
    let second_wallet = EthereumWallet::new(LocalSigner::from_str(
        "0xac1e09fe4f8c7b2e9e13ab632d2f6a77b8cf57fb9f3f35e6c5c7d8f1b2a3c4d5",
    )?);
    let second_signer = ProviderBuilder::new()
        .wallet(second_wallet.clone())
        .connect(tester.l2_rpc_url())
        .await
        .context("failed to connect second signer to L2")?;
    let second_address = second_wallet.default_signer().address();

    // Fund the second wallet so its transaction can remain valid after rebuild.
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(second_address)
                .with_value(U256::from(1_000_000_000_000_000u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    let target_primary_last_block =
        tester.l2_provider.get_block_number().await? + BLOCKS_TO_MINE_BEFORE_REBUILD;
    let mut primary_last_block = tester.l2_provider.get_block_number().await?;
    while primary_last_block < target_primary_last_block {
        let receipt = tester
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(Address::random())
                    .with_value(U256::from(1u64)),
            )
            .await?
            .expect_successful_receipt()
            .await?;
        primary_last_block = receipt
            .block_number
            .expect("transfer receipt should have a block number");
        tokio::time::sleep(TRANSACTION_SEND_INTERVAL).await;
    }
    // Put the second sender into the last historical block so rebuild must preserve at least one
    // unrelated transaction after emptying an older block from the primary sender.
    let second_sender_receipt = second_signer
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;
    let last_rebuilt_block = second_sender_receipt
        .block_number
        .expect("second sender receipt should have a block number");
    let block_to_empty = primary_last_block - BLOCKS_FROM_TIP_TO_EMPTY;

    let original_previous_block_hash = tester
        .l2_provider
        .get_block_by_number((block_to_empty - 1).into())
        .await?
        .context("previous block should exist")?
        .header
        .hash;

    let original_emptied_block_hash = tester
        .l2_provider
        .get_block_by_number(block_to_empty.into())
        .await?
        .context("original block should exist")?
        .header
        .hash;

    let original_last_block_hash = tester
        .l2_provider
        .get_block_by_number(last_rebuilt_block.into())
        .await?
        .context("last block should exist")?
        .header
        .hash;

    let restarted = tester
        .restart_with_overrides(|config| {
            config.sequencer_config.block_rebuild = Some(RebuildBlocksConfig {
                from_block: block_to_empty,
                blocks_to_empty: vec![block_to_empty],
            });
        })
        .await?;
    let rebuild_started_at = Instant::now();

    let rebuilt_last_block = (|| async {
        let rebuilt_last_block = restarted
            .l2_provider
            .get_block_by_number(last_rebuilt_block.into())
            .await?
            .context("rebuilt last block should exist")?;
        let rebuilt_last_block_hash = rebuilt_last_block.header.hash;

        if rebuilt_last_block_hash != original_last_block_hash {
            Ok(rebuilt_last_block)
        } else {
            anyhow::bail!(
                "rebuild not finished yet: last_block={} hash={} original_hash={}",
                last_rebuilt_block,
                rebuilt_last_block_hash,
                original_last_block_hash,
            );
        }
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_millis(200))
            .with_max_times(100),
    )
    .await?;

    let rebuilt_emptied_block = restarted
        .l2_provider
        .get_block_by_number(block_to_empty.into())
        .await?
        .context("rebuilt emptied block should exist")?;
    let rebuilt_previous_block_hash = restarted
        .l2_provider
        .get_block_by_number((block_to_empty - 1).into())
        .await?
        .context("rebuilt previous block should exist")?
        .header
        .hash;
    let rebuilt_emptied_block_tx_count = restarted
        .l2_provider
        .get_block_transaction_count_by_number(block_to_empty.into())
        .await?
        .context("rebuilt emptied block tx count should exist")?;
    let rebuilt_last_tx = restarted
        .l2_provider
        .get_transaction_by_hash(second_sender_receipt.transaction_hash)
        .await?
        .context("rebuilt last transaction should exist")?;
    let rebuilt_emptied_block_hash = rebuilt_emptied_block.header.hash;
    let rebuilt_last_block_hash = rebuilt_last_block.header.hash;
    let rebuild_elapsed = rebuild_started_at.elapsed();

    assert_ne!(
        rebuilt_emptied_block_hash, original_emptied_block_hash,
        "emptied block should be rebuilt with a different hash"
    );
    assert_eq!(
        rebuilt_emptied_block_tx_count, 0,
        "emptied block should be rebuilt without transactions"
    );
    assert_eq!(
        rebuilt_previous_block_hash, original_previous_block_hash,
        "block before the emptied block should remain unchanged"
    );
    assert_ne!(
        rebuilt_last_block_hash, original_last_block_hash,
        "last rebuilt block should have a different hash after rebuild"
    );
    assert_eq!(
        rebuilt_last_tx.block_number,
        Some(last_rebuilt_block),
        "unrelated transaction should remain in the rebuilt last block"
    );

    tracing::info!(
        block_number = last_rebuilt_block,
        "Rebuild finished in {:?}: emptied block {} hash changed {} -> {} and now has {} txs; last rebuilt block {} hash changed {} -> {}; unrelated tx {} ended up in block {:?}",
        rebuild_elapsed, // ~10s at the time of writing this test
        block_to_empty,
        original_emptied_block_hash,
        rebuilt_emptied_block_hash,
        rebuilt_emptied_block_tx_count,
        last_rebuilt_block,
        original_last_block_hash,
        rebuilt_last_block_hash,
        second_sender_receipt.transaction_hash,
        rebuilt_last_tx.block_number,
    );
    Ok(())
}
