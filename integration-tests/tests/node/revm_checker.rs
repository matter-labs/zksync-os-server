use alloy::network::{ReceiptResponse, TransactionBuilder};
use alloy::primitives::{U256, bytes};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, ReceiptAssert};
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
use zksync_os_integration_tests::{CURRENT_TO_L1, NEXT_TO_L1, TestEnvironment, test_multisetup};

/// ZKsync OS hardcodes PREVRANDAO to `1` (the VM's `prevrandao` cargo feature is off in
/// production builds), while `block_context.mix_hash` is zeroed by the sequencer. The REVM
/// consistency checker used to feed `mix_hash` to REVM as prevrandao, so any contract
/// reading `block.prevrandao` tripped a false divergence.
///
/// Runs such a contract with revert-on-divergence enabled: a divergence panics the node,
/// which would fail block finalization below. Only protocol v31+ (execution version 6,
/// AtlasV3) exercised the buggy path, so the NEXT setup is the load-bearing one.
#[test_multisetup([CURRENT_TO_L1, NEXT_TO_L1])]
async fn prevrandao_does_not_trip_revm_consistency_checker(
    env: TestEnvironment,
) -> anyhow::Result<()> {
    let mut config = env.default_config().await?;
    config.sequencer_config.revm_consistency_checker_enabled = true;
    config
        .sequencer_config
        .revm_consistency_checker_revert_on_divergence = true;
    let tester = env.launch(config).await?;

    // Runtime: `PREVRANDAO; PUSH1 0; SSTORE; STOP` — every call stores `block.prevrandao`
    // into slot 0. Init code returns those 5 bytes from the tail of the first memory word.
    let init_code = bytes!("0x6444600055006000526005601bf3");

    let deploy_receipt = tester
        .l2_provider
        .send_transaction(TransactionRequest::default().with_deploy_code(init_code))
        .await?
        .expect_successful_receipt()
        .await?;
    let contract = deploy_receipt
        .contract_address()
        .expect("no contract deployed");

    let call_receipt = tester
        .l2_provider
        .send_transaction(TransactionRequest::default().with_to(contract))
        .await?
        .expect_successful_receipt()
        .await?;

    let stored = tester
        .l2_provider
        .get_storage_at(contract, U256::ZERO)
        .await?;
    assert_eq!(
        stored,
        U256::ONE,
        "ZKsync OS serves a constant `1` for PREVRANDAO"
    );

    // Finalization runs downstream of the consistency checker, so it only completes if the
    // checker processed the block without detecting a divergence.
    tester
        .l2_zk_provider
        .wait_finalized_with_timeout(
            call_receipt
                .block_number()
                .expect("receipt has block number"),
            DEFAULT_TIMEOUT,
        )
        .await?;

    Ok(())
}
