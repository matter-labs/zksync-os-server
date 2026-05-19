use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use alloy::sol;
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, POLL_INTERVAL, ReceiptAssert};
use zksync_os_integration_tests::config::{ChainLayout, load_chain_config};
use zksync_os_integration_tests::dyn_wallet_provider::EthWalletProvider;
use zksync_os_integration_tests::provider::ZksyncApi;
use zksync_os_integration_tests::rpc_recorder::RpcRecordConfig;
use zksync_os_integration_tests::{
    CURRENT_TO_L1, StoppedTester, TestEnvironment, Tester, test_multisetup,
};
use zksync_os_server::config::{Config, RebuildBlocksConfig};

const BLOCKS_TO_MINE_BEFORE_REBUILD: u64 = 10;
const BLOCKS_FROM_TIP_TO_EMPTY: u64 = 4;
const TRANSACTION_SEND_INTERVAL: Duration = Duration::from_millis(5);

async fn fetch_l1_state(tester: &Tester) -> anyhow::Result<L1State> {
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;
    L1State::fetch(
        tester.l1_provider().clone().erased(),
        tester.gateway_eth_provider(),
        bridgehub_address,
        chain_id,
    )
    .await
}

async fn wait_for_l1_state(
    tester: &Tester,
    description: &str,
    predicate: impl Fn(&L1State) -> bool,
) -> anyhow::Result<L1State> {
    let mut retries = DEFAULT_TIMEOUT.div_duration_f64(POLL_INTERVAL).floor() as u64;
    while retries > 0 {
        let state = fetch_l1_state(tester).await?;
        if predicate(&state) {
            return Ok(state);
        }
        retries -= 1;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(anyhow::anyhow!(
        "timed out waiting for L1 state: {description}"
    ))
}

sol! {
    #[sol(rpc)]
    contract ValidatorTimelock {
        function REVERTER_ROLE() external view returns (bytes32);
        function hasRoleForChainId(uint256 _chainId, bytes32 _role, address _address) external view returns (bool);
        function revertBatchesSharedBridge(address _chainAddress, uint256 _newLastBatch) external;
    }
}

#[derive(Debug, Deserialize)]
struct WalletEntry {
    private_key: String,
}

#[derive(Debug, Deserialize)]
struct ChainWallets {
    operator: WalletEntry,
}

fn chain_wallets_path(layout: ChainLayout<'_>, chain_id: u64) -> PathBuf {
    PathBuf::from(
        std::env::var("WORKSPACE_DIR").expect("WORKSPACE_DIR environment variable is not set"),
    )
    .join("local-chains")
    .join(layout.protocol_version())
    .join("multi_chain")
    .join(format!("wallets_{chain_id}.yaml"))
}

fn load_operator_private_key(layout: ChainLayout<'_>, chain_id: u64) -> anyhow::Result<String> {
    let path = chain_wallets_path(layout, chain_id);
    let wallets: ChainWallets = serde_yaml::from_str(&fs::read_to_string(&path)?)?;
    Ok(wallets.operator.private_key)
}

fn make_commit_only_config(config: &mut Config) {
    config.prover_api_config.fake_fri_provers.enabled = true;
    config.prover_api_config.fake_fri_provers.compute_time = Duration::from_millis(200);
    config.prover_api_config.fake_fri_provers.min_age = Duration::ZERO;
    config.prover_api_config.fake_snark_provers.enabled = false;
}

async fn revert_batches_on_l1(stopped: &StoppedTester, new_last_batch: u64) -> anyhow::Result<()> {
    let chain_layout = stopped.chain_layout();
    let chain_config = load_chain_config(stopped.chain_layout()).await;
    let chain_id = chain_config
        .genesis_config
        .chain_id
        .expect("chain config must contain chain id");
    let bridgehub_address = chain_config
        .genesis_config
        .bridgehub_address
        .expect("chain config must contain bridgehub address");
    let bridgehub = Bridgehub::new(bridgehub_address, stopped.l1_provider().clone(), chain_id);
    let validator_timelock_address = bridgehub.validator_timelock_address().await?;
    let chain_address = *bridgehub.zk_chain().await?.address();

    let operator = PrivateKeySigner::from_str(&load_operator_private_key(chain_layout, chain_id)?)?;
    let operator_address = operator.address();
    let mut l1_provider = stopped.l1_provider().clone();
    l1_provider.wallet_mut().register_signer(operator);

    let validator_timelock = ValidatorTimelock::new(validator_timelock_address, l1_provider);
    let reverter_role = validator_timelock.REVERTER_ROLE().call().await?;

    assert!(
        validator_timelock
            .hasRoleForChainId(U256::from(chain_id), reverter_role, operator_address)
            .call()
            .await?,
        "configured operator does not have the reverter role on validator timelock"
    );

    let revert_tx = validator_timelock
        .revertBatchesSharedBridge(chain_address, U256::from(new_last_batch))
        .from(operator_address)
        .send()
        .await?;
    revert_tx.expect_successful_receipt().await?;
    Ok(())
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

    let mut restarted_config = tester.config().clone();
    restarted_config.sequencer_config.block_rebuild = Some(RebuildBlocksConfig {
        from_block: block_to_empty,
        blocks_to_empty: vec![block_to_empty],
        reset_timestamps: false,
    });
    let restarted = tester.restart_with_config(restarted_config).await?;
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

    let rpc_report = rpc_recorder.stop().await;
    rpc_report.assert_eventually_ready()?;
    tracing::info!(
        timeline = %rpc_report.format_detailed_timeline(),
        "Observed HTTP RPC detailed timeline during rebuild"
    );

    Ok(())
}

/// Verifies that the node panics on startup when `block_rebuild.from_block` points to a block
/// that is already committed on L1 (i.e. `from_block <= last_l1_committed_block`).
///
/// Scenario:
///   1. Start a node with the batcher enabled and mine a few blocks until at least one batch
///      is committed to L1.
///   2. Restart with `block_rebuild.from_block = 1`, which is guaranteed to be within the
///      already-committed range.
///   3. Expect a fatal error containing "rebuild_from_block must be > last_l1_committed_block".
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
    let mut restarted_config = tester.config().clone();
    restarted_config.sequencer_config.block_rebuild = Some(RebuildBlocksConfig {
        from_block: 1,
        blocks_to_empty: vec![],
        reset_timestamps: false,
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
        panic_msg.contains("rebuild_from_block must be > last_l1_committed_block"),
        "unexpected panic message: {panic_msg}"
    );

    Ok(())
}

/// Verifies that after reverting committed L1 batches, the node can restart in rebuild mode and
/// process new L2 transactions.
///
/// Without the L1 revert, starting with `block_rebuild.from_block` within the committed range
/// would panic — see `rebuild_panics_if_from_block_is_already_committed` for that assertion.
///
/// Scenario:
///   1. Start a node with the batcher and mine until a batch is committed on L1.
///   2. Stop the node.
///   3. Revert all committed batches on L1 (last_committed_batch → 0).
///   4. Restart the same node in rebuild mode with from_block = 1, which is now valid because
///      from_block (1) > last_l1_committed_block (0) after the revert.
///   5. Confirm the node is alive by sending and confirming a new L2 transaction.
///   6. Verify the server commits a new batch on L1 with the same number as the reverted one.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn rebuild_after_l1_revert_starts_successfully(env: TestEnvironment) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    make_commit_only_config(&mut config);
    let tester = env.launch(config).await?;

    // Unlike `rebuild_panics_if_from_block_is_already_committed` which uses a fast 50ms block
    // time, this test uses the default block time, so we send a transaction to give the batcher
    // real content and trigger a batch commit quickly.
    tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    // last_executed_batch == 0 is a safety measure: since batch execution is disabled, it
    // should always satisfy.
    let committed_state = wait_for_l1_state(
        &tester,
        "a committed but not yet executed batch on L1",
        |state| state.last_committed_batch >= 1 && state.last_executed_batch == 0,
    )
    .await?;

    let stopped = tester.stop().await?;
    // Revert to the last executed batch (0) to reset L1 to an uncommitted state.
    // Without this revert, from_block = 1 would panic at node startup with
    // "rebuild_from_block must be > last_l1_committed_block".
    revert_batches_on_l1(&stopped, committed_state.last_executed_batch).await?;

    // Verify that the revert was successful and last_committed_batch is 0.
    let chain_config = load_chain_config(stopped.chain_layout()).await;
    let chain_id = chain_config
        .genesis_config
        .chain_id
        .expect("chain config must contain chain id");
    let bridgehub_address = chain_config
        .genesis_config
        .bridgehub_address
        .expect("chain config must contain bridgehub address");
    let reverted_state = L1State::fetch(
        stopped.l1_provider().clone().erased(),
        None,
        bridgehub_address,
        chain_id,
    )
    .await?;
    assert_eq!(
        reverted_state.last_committed_batch, 0,
        "all batches should be reverted on L1 before rebuild"
    );

    let mut restart_config = stopped.config().clone();
    restart_config.sequencer_config.block_rebuild = Some(RebuildBlocksConfig {
        from_block: 1,
        blocks_to_empty: vec![],
        reset_timestamps: false,
    });
    let restarted = stopped.start_with_config(restart_config).await?;

    // Confirm the node is alive and accepting new L2 transactions after rebuild.
    restarted
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

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
