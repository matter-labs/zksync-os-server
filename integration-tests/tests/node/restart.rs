use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use anyhow::Context;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, POLL_INTERVAL, ReceiptAssert};
use zksync_os_integration_tests::config::{ChainLayout, load_chain_config};
use zksync_os_integration_tests::dyn_wallet_provider::EthWalletProvider;
use zksync_os_integration_tests::provider::{ZksyncApi, ZksyncTestingProvider};
use zksync_os_integration_tests::{CURRENT_TO_L1, StoppedTester, Tester, test_multisetup};
use zksync_os_server::INTERNAL_CONFIG_FILE_NAME;
use zksync_os_server::config::Config;

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

fn disable_commits_config(config: &mut Config) {
    config.prover_api_config.fake_fri_provers.enabled = false;
    config.prover_api_config.fake_snark_provers.enabled = false;
}

fn make_full_pipeline_config(config: &mut Config) {
    config.prover_api_config.fake_fri_provers.enabled = true;
    config.prover_api_config.fake_fri_provers.compute_time = Duration::from_millis(200);
    config.prover_api_config.fake_fri_provers.min_age = Duration::ZERO;
    config.prover_api_config.fake_snark_provers.enabled = true;
    config.prover_api_config.fake_snark_provers.max_batch_age = Duration::ZERO;
}

fn configure_failing_block(config: &mut Config, failing_block: u64) {
    let internal_config_path = config
        .general_config
        .rocks_db_path
        .join(INTERNAL_CONFIG_FILE_NAME);
    let internal_config = serde_json::json!({
        "failing_block": failing_block,
    });
    std::fs::create_dir_all(
        internal_config_path
            .parent()
            .expect("internal config path must have a parent"),
    )
    .expect("failed to create internal config parent directory");
    std::fs::write(
        &internal_config_path,
        serde_json::to_vec(&internal_config).expect("failed to serialize internal config"),
    )
    .expect("failed to write internal config");
}

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

async fn wait_for_l1_transaction_receipt(
    tester: &Tester,
    description: &str,
    tx_hash: B256,
) -> anyhow::Result<()> {
    let mut retries = DEFAULT_TIMEOUT.div_duration_f64(POLL_INTERVAL).floor() as u64;
    while retries > 0 {
        if tester
            .l1
            .provider
            .get_transaction_receipt(tx_hash)
            .await?
            .is_some()
        {
            return Ok(());
        }
        retries -= 1;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(anyhow::anyhow!(
        "timed out waiting for L1 transaction receipt: {description}"
    ))
}

async fn block_number_by_id(tester: &Tester, block_id: BlockId) -> anyhow::Result<u64> {
    Ok(tester
        .l2_provider
        .get_block_number_by_id(block_id)
        .await?
        .unwrap_or(0))
}

async fn wait_for_block_number_by_id(
    tester: &Tester,
    description: &str,
    block_id: BlockId,
    predicate: impl Fn(u64) -> bool,
) -> anyhow::Result<u64> {
    let mut retries = DEFAULT_TIMEOUT.div_duration_f64(POLL_INTERVAL).floor() as u64;
    while retries > 0 {
        let block_number = block_number_by_id(tester, block_id).await?;
        if predicate(block_number) {
            return Ok(block_number);
        }
        retries -= 1;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(anyhow::anyhow!(
        "timed out waiting for block frontier: {description}"
    ))
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
async fn node_stop_and_restart_preserves_state() -> anyhow::Result<()> {
    let tester = Tester::builder().build().await?;

    // Send a transaction and wait for it to be included.
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
    let tx_hash = receipt.transaction_hash;

    // Restart the same node (same DB, same L1).
    let restarted = tester.restart().await?;
    // Wait for receipt's block to be available. It might not be immediately available because
    // repository DB did not persist the receipt during previous run.
    restarted
        .l2_zk_provider
        .wait_for_block(receipt.block_number.unwrap())
        .await?;

    // The transaction sent before the restart must still be retrievable.
    let recovered = restarted
        .l2_provider
        .get_transaction_receipt(tx_hash)
        .await?
        .expect("transaction receipt should be present after restart");
    assert_eq!(recovered.transaction_hash, tx_hash);

    Ok(())
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn node_recovers_from_l1_batch_revert_after_restart_v30() -> anyhow::Result<()> {
    let tester = Tester::setup_with_overrides(make_commit_only_config).await?;

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

    let committed_state =
        wait_for_l1_state(&tester, "a committed but not executed batch", |state| {
            state.last_committed_batch >= 1 && state.last_executed_batch == 0
        })
        .await?;
    assert_eq!(
        committed_state.last_proved_batch, 0,
        "fake SNARK provers are disabled, so no batch should be proved"
    );

    let safe_before_revert = wait_for_block_number_by_id(
        &tester,
        "the safe block to advance after an L1 commit",
        BlockId::Number(BlockNumberOrTag::Safe),
        |block_number| block_number > 0,
    )
    .await?;
    assert!(safe_before_revert > 0);

    let stopped = tester.stop().await?;
    revert_batches_on_l1(&stopped, committed_state.last_executed_batch).await?;

    let restarted = stopped.start_with_overrides(disable_commits_config).await?;
    let safe_after_revert =
        block_number_by_id(&restarted, BlockId::Number(BlockNumberOrTag::Safe)).await?;
    assert_eq!(
        safe_after_revert, 0,
        "startup after L1 revert must recover the last committed block from L1"
    );
    let finalized_after_revert =
        block_number_by_id(&restarted, BlockId::Number(BlockNumberOrTag::Finalized)).await?;
    assert_eq!(
        finalized_after_revert, 0,
        "startup after L1 revert must keep the executed frontier unchanged"
    );

    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            block_number_by_id(&restarted, BlockId::Number(BlockNumberOrTag::Safe)).await?,
            0,
            "node must not re-process the reverted historical commit event during catch-up"
        );
    }

    let restarted = restarted
        .restart_with_overrides(make_full_pipeline_config)
        .await?;

    let executed_receipt = restarted
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::random())
                .with_value(U256::from(1u64)),
        )
        .await?
        .expect_to_execute()
        .await?;
    let executed_batch = restarted
        .l2_zk_provider
        .wait_batch_number_by_block_number(executed_receipt.block_number.unwrap())
        .await?;
    assert!(
        executed_batch >= 1,
        "post-revert transactions must be assigned to a finalized batch"
    );

    let executed_state = wait_for_l1_state(
        &restarted,
        "a post-revert batch to be committed, proved and executed",
        |state| {
            state.last_committed_batch >= executed_batch
                && state.last_proved_batch >= executed_batch
                && state.last_executed_batch >= executed_batch
        },
    )
    .await?;
    assert!(
        executed_state.last_executed_batch >= executed_batch,
        "post-revert execution should advance normally"
    );

    Ok(())
}

#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn tester_reports_fatal_node_error() -> anyhow::Result<()> {
    let mut tester = Tester::setup_with_overrides(|config| {
        make_full_pipeline_config(config);
        configure_failing_block(config, 1);
    })
    .await?;

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

    let err = tester
        .wait_for_fatal_error_with_timeout(DEFAULT_TIMEOUT)
        .await?;
    let err_text = err.to_string();
    assert!(
        err_text.contains("batch_sink") || err_text.contains("clear_failing_block_config_task"),
        "unexpected fatal error: {err_text}"
    );

    Ok(())
}

/// Verifies that the L1 sender correctly recovers in-flight L1 transactions from a previous
/// session after a restart.
///
/// Relies on `eth_getTransactionByAccountAndNonce` being supported by the L1 provider (Anvil).
/// Without recovery the sender would re-submit a transaction for the same batch, hit a nonce
/// conflict once the original lands, and crash.
#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn node_recovers_in_flight_l1_transactions_after_restart() -> anyhow::Result<()> {
    let tester = Tester::setup_with_overrides(make_full_pipeline_config).await?;

    // Generate L2 activity so the batcher can seal a batch.
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

    // Wait for batch 1 to fully clear the pipeline so there are no leftover prove / execute
    // transactions in flight when we later restart with batch 2's commit still pending.
    let committed_state = wait_for_l1_state(
        &tester,
        "first batch to be committed, proved and executed on L1",
        |state| {
            state.last_committed_batch >= 1
                && state.last_proved_batch >= 1
                && state.last_executed_batch >= 1
        },
    )
    .await?;
    wait_for_l1_block_number(
        &tester,
        "extra L1 confirmations after the first execution",
        |block_number| block_number >= committed_state.sl_block_number + 3,
    )
    .await?;

    // Resolve the commit operator address so we can monitor its pending nonce.
    let config = load_chain_config(ChainLayout::Default {
        protocol_version: CURRENT_TO_L1.protocol_version,
    })
    .await;
    let operator_address = config
        .l1_sender_config
        .operator_commit_sk
        .expect("operator_commit_sk must be configured")
        .address()
        .await?;

    // Snapshot the confirmed nonce before freezing L1 block production.
    let confirmed_nonce = tester
        .l1
        .provider
        .get_transaction_count(operator_address)
        .latest()
        .await?;

    #[derive(Debug, Deserialize)]
    struct TxHashResponse {
        hash: B256,
    }

    // Freeze L1 block production entirely. Anvil is started with `--block-time 1`, which
    // enables interval mining — `anvil_setAutomine` only controls transaction-triggered
    // mining and has no effect on the interval timer. Setting the interval to 0 stops all
    // block production so any commit transaction submitted after this point will remain in
    // the mempool until mining is re-enabled.
    tester
        .l1
        .provider
        .raw_request::<_, ()>("anvil_setIntervalMining".into(), (0u64,))
        .await?;

    // Generate L2 activity for batch 2 so the commit sender has something to submit.
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

    // Wait until the commit transaction for batch 2 appears in the mempool.
    tokio::time::timeout(DEFAULT_TIMEOUT, async {
        loop {
            let pending_nonce = tester
                .l1
                .provider
                .get_transaction_count(operator_address)
                .pending()
                .await?;
            if pending_nonce > confirmed_nonce {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("timed out waiting for in-flight L1 transaction to appear in mempool")?
    .context("polling for in-flight L1 transaction")?;

    let in_flight_tx = tester
        .l1
        .provider
        .raw_request::<_, Option<TxHashResponse>>(
            "eth_getTransactionByAccountAndNonce".into(),
            (operator_address, confirmed_nonce),
        )
        .await?
        .context("pending L1 transaction disappeared before restart")?;

    // Stop the node. The commit transaction for batch 2 remains in Anvil's mempool,
    // simulating a crash mid-flight.
    let stopped = tester.stop().await?;

    // Restart. On startup the L1 sender detects the pending transaction via
    // `eth_getTransactionByAccountAndNonce` and registers a watcher for it instead of
    // re-submitting — which would produce a conflicting transaction and a revert.
    let restarted = stopped
        .start_with_overrides(make_full_pipeline_config)
        .await?;

    // Re-enable interval mining so the pending commit transaction gets included.
    restarted
        .l1
        .provider
        .raw_request::<_, ()>("anvil_setIntervalMining".into(), (1u64,))
        .await?;

    // The exact in-flight transaction captured before restart must be the one that gets mined
    // afterwards, proving the sender recovered it instead of re-submitting a replacement.
    wait_for_l1_transaction_receipt(
        &restarted,
        "the recovered in-flight L1 transaction to be mined after restart",
        in_flight_tx.hash,
    )
    .await?;

    Ok(())
}
