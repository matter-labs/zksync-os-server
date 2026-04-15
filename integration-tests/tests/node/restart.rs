use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
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
