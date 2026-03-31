use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use std::time::Duration;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, ReceiptAssert};
use zksync_os_integration_tests::provider::{ZksyncApi, ZksyncTestingProvider};
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

const TRANSACTIONS_TO_SEND_BEFORE_RESTART: usize = 5;

async fn fetch_l1_state(tester: &Tester) -> anyhow::Result<L1State> {
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let bridgehub_address = tester.l2_zk_provider.get_bridgehub_contract().await?;
    L1State::fetch(
        tester.l1_provider().clone().erased(),
        tester.sl_provider().clone().erased(),
        bridgehub_address,
        chain_id,
    )
    .await
}

/// Verifies that a node running with the batcher disabled can be restarted in normal mode and
/// will commit all previously-accumulated blocks to L1.
///
/// Scenario:
///   1. Start with `batcher.enabled = false` — blocks execute and are stored locally but
///      nothing is batched or submitted to L1.
///   2. Mine several blocks and confirm that L1 commitment count did not move.
///   3. Restart in normal mode (batcher + fake provers enabled by default).
///   4. Wait for the last pre-restart block to be finalized (= executed on L1), proving the
///      node settled all pending blocks after re-enabling the batcher.
#[test_multisetup([CURRENT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn uncommitted_blocks_are_settled_after_batcher_reenabled() -> anyhow::Result<()> {
    let tester = Tester::setup_with_overrides(|config| {
        config.batcher_config.enabled = false;
        config.sequencer_config.block_time = Duration::from_millis(50);
    })
    .await?;

    let initial_committed = fetch_l1_state(&tester).await?.last_committed_batch;

    // Mine several blocks while the batcher is off.
    for _ in 0..TRANSACTIONS_TO_SEND_BEFORE_RESTART {
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
    }
    let last_pre_restart_block = tester.l2_provider.get_block_number().await?;

    // Batcher was disabled — the committed batch count must not have changed.
    let committed_with_batcher_off = fetch_l1_state(&tester).await?.last_committed_batch;
    assert_eq!(
        committed_with_batcher_off, initial_committed,
        "no new batches should be committed while the batcher is disabled"
    );

    // Restart in normal mode. Batcher is enabled by default; fake provers are enabled by
    // default in the test harness (enable_prover = false → fake_*_provers.enabled = true).
    let restarted = tester.restart().await?;

    // The restarted node must pick up all pending uncommitted blocks and settle them on L1.
    // Wait until the last pre-restart block is finalized (= executed on L1).
    restarted
        .l2_zk_provider
        .wait_finalized_with_timeout(last_pre_restart_block, DEFAULT_TIMEOUT)
        .await?;

    // Confirm via L1 state that new batches were actually committed and executed.
    let l1_state_after = fetch_l1_state(&restarted).await?;
    assert!(
        l1_state_after.last_committed_batch > initial_committed,
        "expected new batches to be committed after re-enabling the batcher, \
         but committed batch count did not increase ({initial_committed} -> {})",
        l1_state_after.last_committed_batch,
    );

    Ok(())
}
