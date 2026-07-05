//! Migration-at-height over real nodes: a single-sequencer chain is drained, its
//! state is distributed to a fresh committee (snapshot copy — the v1 migration
//! path), and consensus continues the chain from the agreed anchor. And back:
//! rollback restarts a single sequencer from the same write-ahead log, which is
//! valid at any point because it only ever contained finalized blocks.
//!
//! The guard matrix itself (first start must be exactly at the anchor, era
//! mismatches over existing consensus state refuse to start, single-sequencer
//! refuses over consensus state without acknowledgment) is pinned by unit tests
//! next to the guards; these tests cover the operational flows.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(60);

/// Submits a transfer through the given node and returns (block number, tx hash).
async fn transfer_via(
    node: &Tester,
    recipient: Address,
    value: U256,
) -> anyhow::Result<(u64, alloy::primitives::B256)> {
    let receipt = tokio::time::timeout(CONVERGENCE_TIMEOUT, async {
        node.l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(recipient)
                    .with_value(value),
            )
            .await?
            .expect_successful_receipt()
            .await
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("transaction was not included within {CONVERGENCE_TIMEOUT:?}")
    })??;
    Ok((
        receipt.block_number.expect("included txs have a block"),
        receipt.transaction_hash,
    ))
}

#[test_multisetup([CURRENT_TO_L1])]
async fn single_sequencer_chain_migrates_to_a_committee(main_node: Tester) -> anyhow::Result<()> {
    // The pre-consensus era: ordinary single-sequencer operation with real traffic
    // whose effects must survive the cutover.
    let recipient = Address::repeat_byte(0x61);
    let value = U256::from(7_000_000u64);
    let (pre_migration_block, pre_migration_tx) =
        transfer_via(&main_node, recipient, value).await?;

    // Drain: stop the sequencer. Its write-ahead-log tip becomes the anchor; the
    // harness reads it from the stopped node's database exactly like a migration
    // operator would.
    let stopped = main_node.stop().await?;

    // Cutover: three validators start on copies of the drained chain.
    let cluster = MultiNodeTester::migrate_from(&stopped, 3).await?;
    let anchor = cluster.max_height().await?;
    assert!(
        anchor >= pre_migration_block,
        "the anchor must include pre-migration history",
    );

    // The committee continues the chain past the anchor, in agreement.
    cluster
        .wait_for_block_on_all(anchor + 3, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(anchor + 3).await?;

    // The pre-consensus era is ordinary history on every validator: balances and
    // receipts from before the migration are served as if nothing happened.
    for index in 0..cluster.len() {
        let balance = cluster
            .node(index)
            .l2_provider
            .get_balance(recipient)
            .await?;
        assert_eq!(
            balance, value,
            "validator {index} does not serve pre-migration state",
        );
        assert!(
            cluster
                .node(index)
                .l2_provider
                .get_transaction_receipt(pre_migration_tx)
                .await?
                .is_some(),
            "validator {index} does not serve pre-migration receipts",
        );
    }

    // New traffic flows through the committee — submitted to a NON-batcher
    // validator, finalized by consensus, visible everywhere.
    let (included_at, _) = transfer_via(cluster.node(1), recipient, value).await?;
    assert!(
        included_at > anchor,
        "new traffic lands in the consensus era"
    );
    cluster
        .wait_for_block_on_all(included_at, CONVERGENCE_TIMEOUT)
        .await?;
    cluster.assert_block_hashes_agree(included_at).await?;

    // The finality-certificate trail starts at the anchor (pre-consensus history
    // needs no certificates — the era floor covers it) and must cover the new block.
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        let certified = cluster
            .node(1)
            .status()
            .await?
            .consensus
            .and_then(|consensus| consensus.finality_certified_height);
        if certified.unwrap_or(0) >= included_at {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "certificates did not cover block {included_at} (certified: {certified:?})",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    cluster.shutdown_all().await
}

#[test_multisetup([CURRENT_TO_L1])]
async fn migrated_chain_rolls_back_to_the_single_sequencer(
    main_node: Tester,
) -> anyhow::Result<()> {
    let recipient = Address::repeat_byte(0x62);
    let value = U256::from(3_000_000u64);

    // Cutover, then some consensus-era history worth preserving.
    let stopped = main_node.stop().await?;
    let mut cluster = MultiNodeTester::migrate_from(&stopped, 3).await?;
    let (consensus_era_block, consensus_era_tx) =
        transfer_via(cluster.node(1), recipient, value).await?;
    cluster
        .wait_for_block_on_all(consensus_era_block, CONVERGENCE_TIMEOUT)
        .await?;
    let consensus_tip = cluster.max_height().await?;

    // Rollback: stop the whole committee, then restart the batcher validator as a
    // plain single sequencer — same databases, consensus disabled, the rollback
    // explicitly acknowledged (without the flag the node refuses; that guard is
    // unit-pinned).
    for index in [1, 2, 0] {
        cluster.stop_validator(index).await?;
    }
    cluster
        .start_validator_with_config_overrides(0, |config| {
            config.consensus_config.enabled = false;
            config.consensus_config.acknowledge_rollback = true;
        })
        .await?;

    // The single sequencer produces on demand (it does not seal empty blocks the
    // way the consensus cadence does), so drive it with traffic. Note the resumed
    // chain continues from THIS node's durable tip, which may be a block or two
    // behind the highest height consensus ever finalized: finality (a certificate
    // exists) runs slightly ahead of durability (every validator has committed the
    // block). A real rollback picks the validator with the highest write-ahead-log
    // tip and accepts that the in-flight tail may be dropped — the runbook rule
    // this test documents.
    let post_rollback_recipient = Address::repeat_byte(0x63);
    let (post_rollback_block, _) =
        transfer_via(cluster.node(0), post_rollback_recipient, value).await?;
    assert!(
        post_rollback_block > consensus_era_block,
        "linear production must continue past the durable consensus era          ({post_rollback_block} vs {consensus_era_block}; consensus tip was {consensus_tip})",
    );

    // The consensus era's durable blocks are ordinary chain history to the single
    // sequencer: receipts and state effects are served unchanged.
    assert_eq!(
        cluster.node(0).l2_provider.get_balance(recipient).await?,
        value,
        "the consensus era's state must survive the rollback",
    );
    assert!(
        cluster
            .node(0)
            .l2_provider
            .get_transaction_receipt(consensus_era_tx)
            .await?
            .is_some(),
        "the consensus era's receipts must survive the rollback",
    );

    cluster.shutdown_all().await
}
