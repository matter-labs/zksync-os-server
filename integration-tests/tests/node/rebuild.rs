use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::B256;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionReceipt;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use std::num::NonZeroU64;
use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, ReceiptAssert};
use zksync_os_integration_tests::l1_helpers::{fetch_l1_state, wait_for_l1_state};
use zksync_os_integration_tests::rpc_recorder::RpcRecordConfig;
use zksync_os_integration_tests::test_config::{
    make_commit_only_config, make_full_pipeline_config,
};
use zksync_os_integration_tests::wallets::load_operator_private_key;
use zksync_os_integration_tests::{
    CURRENT_TO_L1, StoppedTester, TestEnvironment, Tester, test_multisetup,
};
use zksync_os_l1_watcher::{fetch_batch, fetch_batch_commit_tx_hash};
use zksync_os_operator_signer::SignerConfig;
use zksync_os_server::config::{RebuildBounds, RebuildConfig};

const BLOCKS_TO_MINE_BEFORE_REBUILD: u64 = 10;
const BLOCKS_FROM_TIP_TO_EMPTY: u64 = 4;
const TRANSACTION_SEND_INTERVAL: Duration = Duration::from_millis(5);

/// Fetches committed batch `batch_number` from L1, returning `(batch_hash, first_block, last_block)`.
async fn fetch_committed_batch(
    tester: &Tester,
    batch_number: u64,
) -> anyhow::Result<(B256, u64, u64)> {
    let l1_state = fetch_l1_state(tester).await?;
    let batch = fetch_batch(
        &l1_state.diamond_proxy_sl,
        batch_number,
        tester.config().l1_watcher_config.max_blocks_to_process,
    )
    .await?;
    Ok((
        batch.batch_info.hash(),
        batch.first_block_number(),
        batch.last_block_number(),
    ))
}

async fn fetch_on_chain_batch_hash(tester: &Tester, batch_number: u64) -> anyhow::Result<B256> {
    Ok(fetch_committed_batch(tester, batch_number).await?.0)
}

/// Fetches the hash of the L1 transaction that currently commits `batch_number`. Used to populate
/// the `l1_revert` guard (`from_batch_commit_tx_hash`).
async fn fetch_on_chain_batch_commit_tx_hash(
    tester: &Tester,
    batch_number: u64,
) -> anyhow::Result<B256> {
    let l1_state = fetch_l1_state(tester).await?;
    fetch_batch_commit_tx_hash(
        &l1_state.diamond_proxy_sl,
        batch_number,
        tester.config().l1_watcher_config.max_blocks_to_process,
    )
    .await
}

/// Returns the hash of L2 block `block_number`, erroring if the block does not exist.
async fn block_hash(tester: &Tester, block_number: u64) -> anyhow::Result<B256> {
    Ok(tester
        .l2_provider
        .get_block_by_number(block_number.into())
        .await?
        .context(format!("block {block_number} should exist"))?
        .header
        .hash)
}

/// Sends a 1-wei transfer to a random address and waits for a successful receipt. Used both to
/// advance a block and to confirm the node is alive and accepting transactions.
async fn send_throwaway_tx(tester: &Tester) -> anyhow::Result<TransactionReceipt> {
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await
}

/// Polls L2 block `block_number` until its hash differs from `original`, returning the new hash.
/// Used by the hash-guard tests to detect that a rebuild completed.
async fn wait_for_block_hash_change(
    tester: &Tester,
    block_number: u64,
    original: B256,
) -> anyhow::Result<B256> {
    (|| async {
        let hash = block_hash(tester, block_number).await?;
        if hash != original {
            Ok(hash)
        } else {
            anyhow::bail!("rebuild not done yet: block {block_number} hash still matches original")
        }
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_millis(200))
            .with_max_times(150),
    )
    .await
}

/// Waits until an in-progress block rebuild has reached `pre_restart_tip` (the chain tip snapshotted
/// before the restart).
async fn wait_for_rebuild_to_reach_tip(
    tester: &Tester,
    pre_restart_tip: u64,
) -> anyhow::Result<()> {
    (|| async {
        let tip = tester.l2_provider.get_block_number().await?;
        if tip >= pre_restart_tip {
            Ok(())
        } else {
            anyhow::bail!("rebuild has not reached the tip yet: {tip} < {pre_restart_tip}")
        }
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_millis(200))
            .with_max_times(150),
    )
    .await
}

fn make_reverter_config(stopped: &StoppedTester) -> anyhow::Result<SignerConfig> {
    let chain_id = stopped
        .config()
        .genesis_config
        .chain_id
        .context("chain_id missing from config")?;
    let operator_sk = load_operator_private_key(stopped.chain_layout(), chain_id)?;
    Ok(SignerConfig::Local(
        PrivateKeySigner::from_str(&operator_sk)?
            .credential()
            .clone(),
    ))
}

/// Sends throwaway L2 transactions on `tester` until `predicate` holds for the L1 state, polling
/// after each send. Used to drive enough blocks/batches onto L1 for the revert scenarios.
///
/// Transactions are sent at a deliberately relaxed cadence: batches seal on the (short) batch
/// timeout, so a slow drip is enough to advance them while keeping the block count — and therefore
/// the per-block Prover Input Generation cost — low.
async fn mine_until_l1_state(
    tester: &Tester,
    description: &str,
    predicate: impl Fn(&L1State) -> bool,
) -> anyhow::Result<L1State> {
    const MINE_SEND_INTERVAL: Duration = Duration::from_millis(500);
    let max_times = DEFAULT_TIMEOUT.div_duration_f64(MINE_SEND_INTERVAL).floor() as u64;
    for _ in 0..max_times {
        let state = fetch_l1_state(tester).await?;
        if predicate(&state) {
            return Ok(state);
        }
        send_throwaway_tx(tester).await?;
        tokio::time::sleep(MINE_SEND_INTERVAL).await;
    }
    Err(anyhow::anyhow!(
        "timed out mining for L1 state: {description}"
    ))
}

#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn rebuild_after_emptying_historical_block_preserves_unrelated_l2_txs(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    {
        config.batcher_config.enabled = false;
        config.sequencer_config.block_time = Duration::from_millis(50);
    }
    let tester = env.launch(config).await?;
    let rpc_recorder = tester.record_l2_http_rpc(RpcRecordConfig::default());

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
        let receipt = send_throwaway_tx(&tester).await?;
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

    let original_previous_block_hash = block_hash(&tester, block_to_empty - 1).await?;
    let original_emptied_block_hash = block_hash(&tester, block_to_empty).await?;
    let original_last_block_hash = block_hash(&tester, last_rebuilt_block).await?;

    let mut restarted_config = tester.config().clone();
    restarted_config.sequencer_config.rebuild = Some(RebuildConfig::BlockRebuild {
        bounds: RebuildBounds {
            from_block_number: block_to_empty,
            from_block_hash: original_emptied_block_hash,
            blocks_to_empty: vec![block_to_empty],
            reset_timestamps: false,
        },
    });
    let restarted = tester.restart_with_config(restarted_config).await?;
    let rebuild_started_at = Instant::now();

    // Wait for the rebuild to reach the tip: the last block's hash changes once it is rebuilt.
    let rebuilt_last_block_hash =
        wait_for_block_hash_change(&restarted, last_rebuilt_block, original_last_block_hash)
            .await?;

    let rebuilt_emptied_block_hash = block_hash(&restarted, block_to_empty).await?;
    let rebuilt_previous_block_hash = block_hash(&restarted, block_to_empty - 1).await?;
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

    let rpc_report = rpc_recorder.stop().await;
    rpc_report.assert_eventually_ready()?;
    tracing::info!(
        timeline = %rpc_report.format_detailed_timeline(),
        "Observed HTTP RPC detailed timeline during rebuild"
    );

    Ok(())
}

/// Verifies that the node panics on startup when `rebuild.from_block_number` points to a block
/// that is already committed on L1 (i.e. `from_block_number <= last_l1_committed_block`).
///
/// Scenario:
///   1. Start a node with the batcher enabled and mine a few blocks until at least one batch
///      is committed to L1.
///   2. Restart with `rebuild.from_block_number = 1`, which is guaranteed to be within the
///      already-committed range.
///   3. Expect a fatal error containing "rebuild_from_block_number must be > last_l1_committed_block".
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn rebuild_panics_if_from_block_is_already_committed(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    config.sequencer_config.block_time = Duration::from_millis(50);
    let tester = env.launch(config).await?;

    // Mine transactions until at least one batch is committed on L1.
    wait_for_l1_state(&tester, "at least one batch committed on L1", |state| {
        state.last_committed_batch >= 1
    })
    .await?;

    // Block 1 is always within the committed range once any batch has been committed.
    let block1_hash = block_hash(&tester, 1).await?;

    let mut restarted_config = tester.config().clone();
    restarted_config.sequencer_config.rebuild = Some(RebuildConfig::BlockRebuild {
        bounds: RebuildBounds {
            from_block_number: 1,
            from_block_hash: block1_hash,
            blocks_to_empty: vec![],
            reset_timestamps: false,
        },
    });

    // The assert! fires synchronously during node startup (before any background tasks are
    // spawned), so it panics through `start_with_config`. Isolate it in a spawned task so
    // the JoinError captures the panic instead of unwinding the test thread.
    let stopped = tester.stop().await?;
    let join_result =
        tokio::task::spawn(async move { stopped.start_with_config(restarted_config).await }).await;

    let join_err = join_result.expect_err("expected node startup to panic");
    assert!(join_err.is_panic(), "expected a panic, got a cancellation");
    let payload = join_err.into_panic();
    let panic_msg = payload
        .downcast_ref::<String>()
        .map(|s| s.as_str())
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("panic payload should be a string");
    assert!(
        panic_msg.contains("rebuild_from_block_number must be > last_l1_committed_block"),
        "unexpected panic message: {panic_msg}"
    );

    Ok(())
}

/// Verifies that the `from_block_hash` guard for `BlockRebuild` skips the rebuild on subsequent
/// restarts after it has already run.
///
/// Scenario:
///   1. Start a node (batcher disabled) and mine a few blocks.
///   2. Snapshot block 1's hash; configure `BlockRebuild` with `reset_timestamps: true` so the
///      rebuilt block gets a new hash, making the guard detectable on the second restart.
///   3. First restart: hash matches → rebuild runs; block 1 gets a new hash.
///   4. Second restart: same config, but block 1's hash has changed → guard fires, rebuild is
///      skipped; node starts normally and accepts new transactions.
#[test_multisetup([CURRENT_TO_L1])]
async fn block_rebuild_hash_guard_prevents_double_rebuild(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    config.batcher_config.enabled = false;
    config.sequencer_config.block_time = Duration::from_millis(50);
    let tester = env.launch(config).await?;

    send_throwaway_tx(&tester).await?;

    let original_block1_hash = block_hash(&tester, 1).await?;

    let stopped = tester.stop().await?;
    let mut restart_config = stopped.config().clone();
    // reset_timestamps: true ensures block 1 gets a new hash after rebuild so the guard
    // can distinguish the pre- and post-rebuild states on the second restart.
    restart_config.sequencer_config.rebuild = Some(RebuildConfig::BlockRebuild {
        bounds: RebuildBounds {
            from_block_number: 1,
            from_block_hash: original_block1_hash,
            blocks_to_empty: vec![],
            reset_timestamps: true,
        },
    });

    // First restart: hash matches → rebuild runs; wait until block 1 has a new hash.
    let first_restart = stopped.start_with_config(restart_config.clone()).await?;
    let rebuilt_block1_hash =
        wait_for_block_hash_change(&first_restart, 1, original_block1_hash).await?;

    // Second restart: block 1's hash has changed → guard fires, rebuild skipped.
    let stopped2 = first_restart.stop().await?;
    let second_restart = stopped2.start_with_config(restart_config).await?;

    // Confirm the node started cleanly and is accepting new transactions.
    send_throwaway_tx(&second_restart).await?;

    // Block 1 must still carry the rebuilt hash — guard fired, no re-rebuild.
    let block1_after = block_hash(&second_restart, 1).await?;
    assert_eq!(
        block1_after, rebuilt_block1_hash,
        "block_rebuild hash guard failed: block 1 was rebuilt again on second restart"
    );

    Ok(())
}

/// Verifies that after reverting committed L1 batches, the node can restart in rebuild mode and
/// process new L2 transactions.
///
/// Without the L1 revert, starting with `rebuild.from_block_number` within the committed range
/// would panic — see `rebuild_panics_if_from_block_is_already_committed` for that assertion.
///
/// Scenario:
///   1. Start a node with the batcher and mine until a batch is committed on L1.
///   2. Stop the node.
///   3. Restart with `rebuild.mode = danger_block_rebuild_with_l1_revert` and
///      `rebuild.from_block_number = 1`; the node reverts all committed batches on L1 and then
///      rebuilds blocks from block 1.
///   4. Confirm the node is alive by sending and confirming a new L2 transaction.
///   5. Verify the server commits a new batch on L1 with the same number as the reverted one.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn rebuild_after_l1_revert_starts_successfully(env: TestEnvironment) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    make_commit_only_config(&mut config);
    let tester = env.launch(config).await?;

    // Unlike `rebuild_panics_if_from_block_is_already_committed` which uses a fast 50ms block
    // time, this test uses the default block time, so we send a transaction to give the batcher
    // real content and trigger a batch commit quickly.
    send_throwaway_tx(&tester).await?;

    let committed_state = wait_for_l1_state(&tester, "a committed batch on L1", |state| {
        state.last_committed_batch >= 1
    })
    .await?;
    // Batch execution is disabled, so nothing should ever be executed.
    assert_eq!(
        committed_state.last_executed_batch, 0,
        "batch execution is disabled, so no batch should be executed"
    );

    // last_executed_batch == 0 so we want to keep batch 0, reverting all committed batches
    // (batch 1+). Batch 1 always starts at block 1, so from_block_number = 1.
    let block1_hash = block_hash(&tester, 1).await?;
    let pre_restart_tip = tester.l2_provider.get_block_number().await?;

    let stopped = tester.stop().await?;
    let reverter_signer = make_reverter_config(&stopped)?;
    let mut restart_config = stopped.config().clone();
    restart_config.sequencer_config.rebuild = Some(RebuildConfig::DangerBlockRebuildWithL1Revert {
        bounds: RebuildBounds {
            from_block_number: 1,
            from_block_hash: block1_hash,
            blocks_to_empty: vec![],
            reset_timestamps: false,
        },
        l1_reverter_sk: reverter_signer,
    });
    let restarted = stopped.start_with_config(restart_config).await?;

    // Wait for the async block rebuild to complete.
    wait_for_rebuild_to_reach_tip(&restarted, pre_restart_tip).await?;

    // Confirm the node is alive and accepting new L2 transactions after rebuild.
    send_throwaway_tx(&restarted).await?;

    // Verify the server commits a new batch on L1 with the same number as the reverted one.
    // After the revert, last_committed_batch on L1 is 0; reaching committed_state.last_committed_batch
    // again proves the node rebuilt and committed a distinct batch with the same number.
    wait_for_l1_state(
        &restarted,
        "server commits a new batch on L1 after rebuild",
        |state| state.last_committed_batch >= committed_state.last_committed_batch,
    )
    .await?;

    Ok(())
}

/// Verifies that `from_block_hash` prevents a second L1 revert when the same config is reused
/// on restart after a successful revert+rebuild.
///
/// Scenario:
///   1. Start a node with the batcher and wait for a committed batch.
///   2. Snapshot block 1's hash (`original_hash`) — this is the guard value.
///   3. First restart: `danger_block_rebuild_with_l1_revert` mode, `from_block_hash = original_hash`.
///      Hash matches → L1 revert + rebuild run; block 1 gets a new hash.
///   4. Wait for the node to commit a new batch on L1 (rebuild complete).
///   5. Second restart with the exact same config.
///      `from_block_hash = original_hash` no longer matches block 1's rebuilt hash → skip revert.
///   6. Assert `last_committed_batch` did not drop back to 0 on the second startup.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn danger_block_rebuild_with_l1_revert_hash_guard_prevents_double_revert(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    make_commit_only_config(&mut config);
    let tester = env.launch(config).await?;

    send_throwaway_tx(&tester).await?;

    let committed_state = wait_for_l1_state(&tester, "at least one committed batch", |state| {
        state.last_committed_batch >= 1
    })
    .await?;
    assert_eq!(
        committed_state.last_executed_batch, 0,
        "batch execution is disabled, so no batch should be executed"
    );

    // Snapshot block 1's hash before any rebuild; this is stored as the guard value.
    let original_block1_hash = block_hash(&tester, 1).await?;

    let stopped = tester.stop().await?;
    let reverter_signer = make_reverter_config(&stopped)?;
    let mut restart_config = stopped.config().clone();
    // reset_timestamps: true ensures the rebuilt block gets a fresh timestamp, so its hash
    // differs from the pre-rebuild value and the guard can distinguish the two states.
    restart_config.sequencer_config.rebuild = Some(RebuildConfig::DangerBlockRebuildWithL1Revert {
        bounds: RebuildBounds {
            from_block_number: 1,
            from_block_hash: original_block1_hash,
            blocks_to_empty: vec![],
            reset_timestamps: true,
        },
        l1_reverter_sk: reverter_signer,
    });

    // First restart: hash matches → L1 revert + rebuild proceed.
    let first_restart = stopped.start_with_config(restart_config.clone()).await?;

    // Wait for block 1 to be rebuilt with a new timestamp (hash changes).
    wait_for_block_hash_change(&first_restart, 1, original_block1_hash).await?;

    // Wait for the batcher to re-commit a batch after rebuild; record the count.
    let state_before_second_restart = wait_for_l1_state(
        &first_restart,
        "new batch committed on L1 after rebuild",
        |state| state.last_committed_batch >= 1,
    )
    .await?;
    let committed_before = state_before_second_restart.last_committed_batch;

    // Second restart: same config, but block 1's hash has changed → guard fires, revert skipped.
    let stopped2 = first_restart.stop().await?;
    let second_restart = stopped2.start_with_config(restart_config).await?;

    // Confirm node is alive.
    send_throwaway_tx(&second_restart).await?;

    // The guard must have fired: L1 was not reverted, so committed_batch did not drop.
    let l1_after = fetch_l1_state(&second_restart).await?;
    assert!(
        l1_after.last_committed_batch >= committed_before,
        "from_block_hash guard failed: L1 revert ran again on second restart \
         (last_committed_batch dropped from {committed_before} to {})",
        l1_after.last_committed_batch,
    );

    Ok(())
}

/// Verifies that `rebuild.mode = l1_revert` reverts committed L1 batches without touching local
/// blocks.
///
/// Scenario:
///   1. Start a node with the batcher and wait for a batch to be committed on L1.
///   2. Snapshot the local tip block hash.
///   3. Restart with `rebuild.mode = l1_revert`, `from_batch_number = 1` (revert batch 1 and above).
///   4. Assert the tip block hash is unchanged (local blocks not rebuilt).
///   5. Assert the node is alive and accepting new L2 transactions.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn revert_l1_commits_without_rebuild_leaves_local_blocks_intact(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    make_commit_only_config(&mut config);
    let tester = env.launch(config).await?;

    send_throwaway_tx(&tester).await?;

    let committed_state = wait_for_l1_state(&tester, "at least one committed batch", |state| {
        state.last_committed_batch >= 1
    })
    .await?;
    assert_eq!(
        committed_state.last_executed_batch, 0,
        "batch execution is disabled, so no batch should be executed"
    );

    // Snapshot the current tip hash; it must survive the standalone L1 revert unchanged.
    let tip_block = tester.l2_provider.get_block_number().await?;
    let original_tip_hash = block_hash(&tester, tip_block).await?;

    // Fetch the commit tx hash of batch 1 for the revert guard.
    let batch1_commit_tx_hash = fetch_on_chain_batch_commit_tx_hash(&tester, 1).await?;

    let stopped = tester.stop().await?;
    let reverter_signer = make_reverter_config(&stopped)?;
    let mut revert_config = stopped.config().clone();
    // Revert batch 1 and above, keeping no committed batches. L1Revert → no local block rebuild.
    revert_config.sequencer_config.rebuild = Some(RebuildConfig::L1Revert {
        from_batch_number: NonZeroU64::new(1).unwrap(),
        from_batch_commit_tx_hash: batch1_commit_tx_hash,
        l1_reverter_sk: reverter_signer,
    });

    let reverted = stopped.start_with_config(revert_config).await?;

    // Confirm the node is alive.
    send_throwaway_tx(&reverted).await?;

    // Local blocks must be unchanged: tip hash identical to pre-revert snapshot.
    let tip_hash_after = block_hash(&reverted, tip_block).await?;
    assert_eq!(
        tip_hash_after, original_tip_hash,
        "local blocks must not change after l1_revert"
    );

    Ok(())
}

/// Verifies that restarting twice with `rebuild.mode = l1_revert` is idempotent: the second
/// startup skips the revert gracefully rather than panicking.
///
/// Before this change the startup path had an `assert!` that fired when
/// `last_committed_batch < from_batch_number`, causing a crash-loop on every pod restart
/// after a successful revert (if no new batches had been committed in the meantime).
///
/// Scenario:
///   1. Commit a batch on L1.
///   2. First restart: `rebuild.mode = l1_revert`, `from_batch_number = 1`, batcher disabled so nothing
///      re-commits; `last_committed_batch` drops to 0.
///   3. Second restart: same config; `last_committed_batch (0) < from_batch_number (1)` → graceful skip.
///   4. Assert the node starts cleanly and processes new L2 transactions.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn revert_l1_commits_without_rebuild_is_idempotent_on_restart(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    make_commit_only_config(&mut config);
    let tester = env.launch(config).await?;

    send_throwaway_tx(&tester).await?;

    let committed_state = wait_for_l1_state(&tester, "at least one committed batch", |state| {
        state.last_committed_batch >= 1
    })
    .await?;
    assert_eq!(
        committed_state.last_executed_batch, 0,
        "batch execution is disabled, so no batch should be executed"
    );

    // Fetch the commit tx hash of batch 1 for the revert guard.
    let batch1_commit_tx_hash = fetch_on_chain_batch_commit_tx_hash(&tester, 1).await?;

    let stopped = tester.stop().await?;
    let reverter_signer = make_reverter_config(&stopped)?;
    let mut revert_config = stopped.config().clone();
    // Disable the batcher so last_committed_batch stays at 0 after the revert, giving the
    // idempotency test a stable condition to exercise the graceful-skip path.
    revert_config.batcher_config.enabled = false;
    // Revert batch 1 and above; L1Revert mode means no local block rebuild runs.
    revert_config.sequencer_config.rebuild = Some(RebuildConfig::L1Revert {
        from_batch_number: NonZeroU64::new(1).unwrap(),
        from_batch_commit_tx_hash: batch1_commit_tx_hash,
        l1_reverter_sk: reverter_signer,
    });

    // First restart: revert runs (last_committed_batch > 0).
    let first_reverted = stopped.start_with_config(revert_config.clone()).await?;

    // Verify the revert ran: wait until last_committed_batch reaches 0.
    wait_for_l1_state(&first_reverted, "all committed batches reverted", |state| {
        state.last_committed_batch == 0
    })
    .await?;

    // Second restart: last_committed_batch (0) < from_batch_number (1) → graceful skip, no panic.
    let stopped2 = first_reverted.stop().await?;
    let second = stopped2.start_with_config(revert_config).await?;

    // Confirm the node started cleanly and is alive.
    send_throwaway_tx(&second).await?;

    Ok(())
}

/// Verifies `danger_block_rebuild_with_l1_revert` when `from_block_number` lies in a committed batch
/// that is **not** the last committed batch.
///
/// The `from_block_number = 1` tests always resolve to batch 1 in a single scan step, so they never
/// exercise `derive_last_l1_batch_to_keep`'s scan loop (skipping higher committed batches and
/// decrementing). This test commits at least three batches and points `from_block_number` into batch
/// 2, forcing the scan to skip every batch above 2 before deciding to keep batch 1 and revert batch 2
/// and above. When batch 2 happens to span multiple blocks, `from_block_number` is placed strictly
/// inside it, additionally covering mid-batch acceptance.
///
/// Scenario:
///   1. Start a node with the batcher and a short batch timeout so several batches commit.
///   2. Mine until `last_committed_batch >= 3` (still unexecuted) — guarantees batch 2 is a
///      non-last committed batch with batch 1 below it to keep and batches above it to skip.
///   3. Point `from_block_number` into batch 2 (strictly inside it if it spans >1 block). Snapshot
///      the on-chain hash of batch 1 (must survive) and a local block hash below `from_block_number`.
///   4. Restart with `danger_block_rebuild_with_l1_revert` from that `from_block_number`.
///   5. Assert: the node started (no panic — the startup `from_block_number > last_l1_committed_block`
///      assertion only holds if the revert kept exactly batch 1), batch 1's on-chain hash is
///      unchanged, the local block below `from_block_number` is unchanged, the node accepts new L2
///      transactions, and it re-commits up to at least the reverted batch.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn danger_block_rebuild_with_l1_revert_from_mid_batch(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    make_commit_only_config(&mut config);
    let tester = env.launch(config).await?;

    // Drive at least three committed (still unexecuted) batches onto L1 so batch 2 is a non-last
    // committed batch (batch 1 below to keep, batches >= 3 above to skip during the scan).
    let committed_state =
        mine_until_l1_state(&tester, "at least three committed batches", |state| {
            state.last_committed_batch >= 3
        })
        .await?;
    assert_eq!(
        committed_state.last_executed_batch, 0,
        "batch execution is disabled, so no batch should be executed"
    );

    // Target batch 2: a non-last committed batch. Place `from_block_number` strictly inside it when
    // it spans multiple blocks, otherwise at its single block — either way it resolves to batch 2.
    let containing_batch = 2;
    let (_, first_block, last_block) = fetch_committed_batch(&tester, containing_batch).await?;
    let from_block_number = if last_block > first_block {
        first_block + 1
    } else {
        first_block
    };
    // Batch 1 is the deepest batch that must survive the revert.
    let survivor_batch = containing_batch - 1;

    let from_block_hash = block_hash(&tester, from_block_number).await?;
    // A local block strictly below `from_block_number` — must be untouched by the rebuild.
    let below_block = from_block_number - 1;
    let below_block_hash = block_hash(&tester, below_block).await?;
    // On-chain hash of the surviving batch — must be unchanged after the revert+rebuild, proving
    // the revert kept everything up to and including `survivor_batch`.
    let survivor_hash_before = fetch_on_chain_batch_hash(&tester, survivor_batch).await?;
    let pre_restart_tip = tester.l2_provider.get_block_number().await?;

    let stopped = tester.stop().await?;
    let reverter_signer = make_reverter_config(&stopped)?;
    let mut restart_config = stopped.config().clone();
    restart_config.sequencer_config.rebuild = Some(RebuildConfig::DangerBlockRebuildWithL1Revert {
        bounds: RebuildBounds {
            from_block_number,
            from_block_hash,
            blocks_to_empty: vec![],
            reset_timestamps: false,
        },
        l1_reverter_sk: reverter_signer,
    });
    // Startup panics if the derived revert boundary is wrong (`from_block_number` would not be
    // strictly greater than `last_l1_committed_block`), so a successful start already proves the derivation.
    let restarted = stopped.start_with_config(restart_config).await?;

    // Wait for the async block rebuild to complete.
    wait_for_rebuild_to_reach_tip(&restarted, pre_restart_tip).await?;

    // The surviving batch must be byte-for-byte identical: it was never reverted.
    let survivor_hash_after = fetch_on_chain_batch_hash(&restarted, survivor_batch).await?;
    assert_eq!(
        survivor_hash_after, survivor_hash_before,
        "batch {survivor_batch} below the containing batch must survive the mid-batch revert unchanged"
    );

    // Local blocks below `from_block_number` must be untouched by the rebuild.
    let below_block_hash_after = block_hash(&restarted, below_block).await?;
    assert_eq!(
        below_block_hash_after, below_block_hash,
        "block {below_block} below from_block_number must not change during rebuild"
    );

    // Node is alive and accepting new L2 transactions after the rebuild.
    send_throwaway_tx(&restarted).await?;

    // The node rebuilds and re-commits at least up to the batch that was reverted.
    wait_for_l1_state(
        &restarted,
        "node re-commits up to the reverted batch after mid-batch rebuild",
        move |state| state.last_committed_batch >= containing_batch,
    )
    .await?;

    Ok(())
}

/// Verifies that `rebuild.mode = l1_revert` refuses to revert a batch that has already been
/// executed (finalized) on L1.
///
/// Scenario:
///   1. Start a node with the full pipeline and wait until at least one batch is executed on L1.
///   2. Restart with `rebuild.mode = l1_revert`, `from_batch_number = 1` (<= last_executed_batch).
///   3. Expect a fatal startup error containing "at or before the last executed batch".
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn l1_revert_rejects_already_executed_batch(env: TestEnvironment) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    make_full_pipeline_config(&mut config);
    let tester = env.launch(config).await?;

    send_throwaway_tx(&tester).await?;

    // The full pipeline executes batches; wait until batch 1 is executed (finalized) on L1.
    wait_for_l1_state(&tester, "at least one executed batch on L1", |state| {
        state.last_executed_batch >= 1
    })
    .await?;

    // Pass the real commit tx hash so the test stays meaningful even if the guard order changes:
    // with a matching hash the revert proceeds to the executed-batch check either way.
    let batch1_commit_tx_hash = fetch_on_chain_batch_commit_tx_hash(&tester, 1).await?;

    let stopped = tester.stop().await?;
    let reverter_signer = make_reverter_config(&stopped)?;
    let mut revert_config = stopped.config().clone();
    // from_batch_number = 1 is at or below last_executed_batch, so the revert must be rejected.
    revert_config.sequencer_config.rebuild = Some(RebuildConfig::L1Revert {
        from_batch_number: NonZeroU64::new(1).unwrap(),
        from_batch_commit_tx_hash: batch1_commit_tx_hash,
        l1_reverter_sk: reverter_signer,
    });

    // The guard fires synchronously during startup via `handle_startup_rebuild(...).expect(...)`,
    // so it panics through `start_with_config`. Isolate it in a spawned task so the JoinError
    // captures the panic instead of unwinding the test thread.
    let join_result =
        tokio::task::spawn(async move { stopped.start_with_config(revert_config).await }).await;

    let join_err = join_result.expect_err("expected node startup to panic");
    assert!(join_err.is_panic(), "expected a panic, got a cancellation");
    let payload = join_err.into_panic();
    let panic_msg = payload
        .downcast_ref::<String>()
        .map(|s| s.as_str())
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("panic payload should be a string");
    assert!(
        panic_msg.contains("at or before the last executed batch"),
        "unexpected panic message: {panic_msg}"
    );

    Ok(())
}
