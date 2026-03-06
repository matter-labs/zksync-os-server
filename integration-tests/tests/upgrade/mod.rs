use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, FixedBytes, TxKind, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use std::collections::BTreeMap;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::SampleForceDeployment;
use zksync_os_integration_tests::upgrade::{Action, CommitterFacetV31, FacetCut, UpgradeTester};

/// Executes the simplest patch protocol upgrade:
/// - no contracts are deployed
/// - patch version is bumped by 1
/// - upgrade timestamp is 0
/// Importance of this test: unlike minor version upgrades, patch upgrades
/// do not include an upgrade transaction in the block. Hence, we need to ensure that
/// the system can handle patch upgrades correctly.
#[test_log::test(tokio::test)]
async fn upgrade_patch_no_deployments() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(0); // Protocol upgrade can be executed immediately.
    let deadline = U256::MAX; // The protocol version will not have any deadline in this upgrade

    // Test that we can deposit L2 funds from a rich L1 account
    let tester = Tester::setup().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;

    // Prepare protocol upgrade
    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_patch(1)
        .with_force_deployments(BTreeMap::new())
        .with_timestamp(upgrade_timestamp)
        .build();

    upgrade_tester
        .execute_default_upgrade(
            &protocol_upgrade,
            deadline,
            upgrade_timestamp,
            true,
            Vec::new(),
        )
        .await?;

    Ok(())
}

/// Performs V30->V31 protocol upgrade which also does a force deployment.
///
/// This test verifies the full upgrade flow including bytecodes supplier integration:
/// the `DEPLOYED_BYTECODE` is published to the `BytecodesSupplier` contract on L1 so
/// the server can fetch it as a force preimage via `EVMBytecodePublished` events.
///
/// Note: the contract is also deployed on L2 directly to ensure its ZKsync OS preimage
/// (keyed by `blake2s256(bytecode + artifacts)`) is available for post-upgrade EVM
/// execution. Once the protocol aligns the on-chain and server-side hash formats,
/// the direct L2 deployment step will no longer be necessary.
#[test_log::test(tokio::test)]
async fn upgrade_to_v31_with_deployments() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(0); // Protocol upgrade can be executed immediately.
    let deadline = U256::MAX; // The protocol version will not have any deadline in this upgrade

    let sample_force_deployment_address: Address = "0x000000000000000000000000000000000000dead"
        .parse()
        .unwrap();

    let force_deployments: BTreeMap<Address, Bytes> = [(
        sample_force_deployment_address,
        SampleForceDeployment::DEPLOYED_BYTECODE.clone(),
    )]
    .into_iter()
    .collect();

    let tester = Tester::setup().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;

    // Publish the deployed bytecode to the BytecodesSupplier on L1 so the server
    // collects it as a force preimage via `EVMBytecodePublished` events.
    upgrade_tester
        .publish_bytecodes([SampleForceDeployment::DEPLOYED_BYTECODE.clone()])
        .await?;

    // Also deploy on L2 so the ZKsync OS preimage (blake2s256 of bytecode+artifacts)
    // is available in state for post-upgrade EVM execution.
    upgrade_tester
        .tester
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_kind(TxKind::Create)
                .with_input(SampleForceDeployment::BYTECODE.clone()),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    // Prepare protocol upgrade
    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_minor(1)
        .with_force_deployments(force_deployments)
        .with_timestamp(upgrade_timestamp)
        .build();

    // Deploy new CommitterFacet.
    let l1_chain_id = upgrade_tester.tester.l1_provider().get_chain_id().await?;
    let committer_facet = CommitterFacetV31::deploy(
        upgrade_tester.tester.l1_provider().clone(),
        U256::from(l1_chain_id),
    )
    .await?;

    // For simplicity, we only do a replacement for `commitBatchesSharedBridge`.
    let facet_cut = FacetCut {
        facet: *committer_facet.address(),
        action: Action::Replace,
        isFreezable: true,
        selectors: vec![FixedBytes(
            CommitterFacetV31::commitBatchesSharedBridgeCall::SELECTOR,
        )],
    };

    upgrade_tester
        .execute_default_upgrade(
            &protocol_upgrade,
            deadline,
            upgrade_timestamp,
            false,
            vec![facet_cut],
        )
        .await?;

    // Ensure that the contract is now callable.
    let force_deployed_contract = SampleForceDeployment::new(
        sample_force_deployment_address,
        upgrade_tester.tester.l2_provider.clone(),
    );
    let stored_value = force_deployed_contract.return42().call().await?;
    assert_eq!(stored_value, U256::from(42));

    let main_node_block = upgrade_tester.tester.l2_provider.get_block_number().await?;

    // Ensure that EN can sync from the upgraded node.
    let en1 = upgrade_tester.tester.launch_external_node().await?;

    while en1.l2_provider.get_block_number().await? < main_node_block {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Ok(())
}

/// Verifies that bytecodes published to the `BytecodesSupplier` contract on L1 are
/// correctly fetched by the server during a protocol upgrade.
///
/// The test publishes a bytecode to the supplier before executing a patch upgrade,
/// verifying that:
/// 1. `EVMBytecodePublished` events on L1 are scanned and returned as force preimages.
/// 2. The upgrade completes successfully even when the supplier is active.
///
/// Since this is a patch-only upgrade (no force deployments), it focuses on confirming
/// that the supplier scanning path does not interfere with the upgrade flow.
#[test_log::test(tokio::test)]
async fn upgrade_with_bytecodes_from_supplier() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(0);
    let deadline = U256::MAX;

    let tester = Tester::setup().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;

    // Publish a bytecode to the supplier contract on L1 before the upgrade.
    // The server should scan `EVMBytecodePublished` events and return this bytecode
    // as a force preimage when processing the upgrade.
    upgrade_tester
        .publish_bytecodes([SampleForceDeployment::DEPLOYED_BYTECODE.clone()])
        .await?;

    // Execute a patch upgrade. Patch upgrades do not include an L2 upgrade transaction
    // so force preimages are not consumed, but the server must still scan the supplier
    // without errors.
    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_patch(1)
        .with_force_deployments(BTreeMap::new())
        .with_timestamp(upgrade_timestamp)
        .build();

    upgrade_tester
        .execute_default_upgrade(
            &protocol_upgrade,
            deadline,
            upgrade_timestamp,
            true,
            Vec::new(),
        )
        .await?;

    Ok(())
}
