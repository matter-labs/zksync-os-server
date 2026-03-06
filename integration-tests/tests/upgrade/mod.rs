use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use std::collections::BTreeMap;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::contracts::{EventEmitter, SampleForceDeployment};
use zksync_os_integration_tests::upgrade::{
    Action, CommitterFacetV31, DefaultUpgrade, FacetCut, UpgradeTester,
};
use zksync_os_types::ProtocolSemanticVersion;

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

    // Test that we can deposit L2 funds from a rich L1 account
    let tester = Tester::setup().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;

    // Publish the bytecodes for upgrade beforehand.
    // TODO: we need to use bytecode instead of deployed bytecode for now, since under the hood `publish_bytecodes`
    // actually deploys contracts since BytecodesSupplier is not ready for zksync os
    // Once this is fixed, also check the logic for `ForceDeploymentBytecodeInfo` in the builder.
    upgrade_tester
        .publish_bytecodes([SampleForceDeployment::BYTECODE.clone()])
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

/// Tests that the l1_watcher correctly handles a chain that skips an intermediate version,
/// and that force deployments are applied in the correct order across multiple minor upgrades.
///
/// Scenario:
///   - Chain starts at v30.2.
///   - v30.3 (patch) is registered on CTM but NO timestamp is set — the chain skips it.
///   - v31.0 (minor) is registered with a timestamp. The l1_watcher detects v30.3 as an
///     intermediate via `fetch_intermediate_upgrade_infos` and submits both to UpgradeSubpool.
///     The v31.0 upgrade force-deploys GIBBERISH bytecode to the target address.
///   - v32.0 (minor) is registered; it force-deploys the CORRECT `SampleForceDeployment`.
///
/// After both upgrades are finalized, `SampleForceDeployment.return42()` must return 42,
/// confirming that the second upgrade's force deployment overwrote the gibberish.
#[test_log::test(tokio::test)]
async fn upgrade_skip_intermediate_patch_then_minor() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(0); // Upgrade can be executed immediately.
    let deadline = U256::MAX; // No deadline for old protocol version.

    let sample_force_deployment_address: Address = "0x000000000000000000000000000000000000dead"
        .parse()
        .unwrap();

    let tester = Tester::setup().await?;
    let mut upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;

    // ======= PHASE 1: Skip v30.3 patch, upgrade to v31.0 with gibberish force deployment =======

    // Register the intermediate patch upgrade (v30.2 → v30.3) on CTM without a timestamp.
    // The chain skips this version; the l1_watcher detects it as intermediate cut data.
    let intermediate_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_patch(1)
        .with_force_deployments(BTreeMap::new())
        .with_timestamp(upgrade_timestamp)
        .build();

    let intermediate_upgrade_contract =
        DefaultUpgrade::deploy(upgrade_tester.tester.l1_provider(), &intermediate_upgrade).await?;
    let intermediate_cut_data = intermediate_upgrade_contract.diamond_cut_data(vec![]);

    upgrade_tester.pause_bridgehub_migrations().await?;
    upgrade_tester
        .set_new_version_on_ctm(
            intermediate_cut_data,
            deadline,
            intermediate_upgrade.newProtocolVersion,
        )
        .await?;
    tracing::info!(
        intermediate_version = ?intermediate_upgrade.newProtocolVersion,
        "Intermediate patch (v30.3) registered on CTM without timestamp"
    );

    // Publish gibberish bytecode (EventEmitter) so its preimage is known on L2.
    upgrade_tester
        .publish_bytecodes([EventEmitter::BYTECODE.clone()])
        .await?;

    // Build the v31.0 minor upgrade with GIBBERISH force deployment to the target address.
    let gibberish_deployments: BTreeMap<Address, Bytes> = [(
        sample_force_deployment_address,
        EventEmitter::DEPLOYED_BYTECODE.clone(),
    )]
    .into_iter()
    .collect();

    let v31_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_minor(1)
        .with_force_deployments(gibberish_deployments)
        .with_timestamp(upgrade_timestamp)
        .build();

    let v31_upgrade_contract =
        DefaultUpgrade::deploy(upgrade_tester.tester.l1_provider(), &v31_upgrade).await?;

    // Install CommitterFacetV31 as part of the v31.0 diamond cut so the chain can commit
    // batches encoded with the V31 format after the upgrade.
    let l1_chain_id = upgrade_tester.tester.l1_provider().get_chain_id().await?;
    let committer_facet = CommitterFacetV31::deploy(
        upgrade_tester.tester.l1_provider().clone(),
        U256::from(l1_chain_id),
    )
    .await?;
    let v31_facet_cut = FacetCut {
        facet: *committer_facet.address(),
        action: Action::Replace,
        isFreezable: true,
        selectors: vec![FixedBytes(
            CommitterFacetV31::commitBatchesSharedBridgeCall::SELECTOR,
        )],
    };
    let v31_cut_data = v31_upgrade_contract.diamond_cut_data(vec![v31_facet_cut]);

    // Register v31.0 on CTM and set its upgrade timestamp.
    // The l1_watcher will detect the v30.3 intermediate via fetch_intermediate_upgrade_infos
    // and enqueue both upgrades (v30.3 patch then v31.0 minor) into the UpgradeSubpool.
    upgrade_tester
        .set_new_version_on_ctm(v31_cut_data.clone(), deadline, v31_upgrade.newProtocolVersion)
        .await?;
    upgrade_tester
        .set_upgrade_timestamp(v31_upgrade.newProtocolVersion, upgrade_timestamp)
        .await?;
    tracing::info!(
        v31_version = ?v31_upgrade.newProtocolVersion,
        "v31.0 upgrade registered on CTM with timestamp"
    );

    // Wait for the v31.0 upgrade tx to land on L2.
    // The sequencer first applies the v30.3 patch (no tx in block), then the v31.0 upgrade tx.
    upgrade_tester
        .wait_for_upgrade(v31_upgrade_contract.upgrade_tx_l2_hash())
        .await?;
    tracing::info!("v31.0 upgrade tx confirmed on L2, executing L1 upgrade");

    // Execute the L1 upgrade from v30.2 → v31.0 (installs CommitterFacetV31 + gibberish).
    upgrade_tester.upgrade_chain(v31_cut_data).await?;
    upgrade_tester
        .wait_for_upgrade_finalization(v31_upgrade_contract.upgrade_tx_l2_hash())
        .await?;
    tracing::info!("v31.0 upgrade (with gibberish force deployment) fully finalized");

    // ======= PHASE 2: Upgrade v31.0 → v32.0 with correct SampleForceDeployment =======

    // Update the tester's tracked protocol_version to v31.0 so subsequent calls to
    // set_new_version_on_ctm and upgrade_chain use it as the old/current version.
    upgrade_tester.protocol_version = ProtocolSemanticVersion::new(0, 31, 0);

    // Publish the correct SampleForceDeployment bytecode.
    upgrade_tester
        .publish_bytecodes([SampleForceDeployment::BYTECODE.clone()])
        .await?;

    let correct_deployments: BTreeMap<Address, Bytes> = [(
        sample_force_deployment_address,
        SampleForceDeployment::DEPLOYED_BYTECODE.clone(),
    )]
    .into_iter()
    .collect();

    let v32_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_minor(1)
        .with_force_deployments(correct_deployments)
        .with_timestamp(upgrade_timestamp)
        .build();

    let v32_upgrade_contract =
        DefaultUpgrade::deploy(upgrade_tester.tester.l1_provider(), &v32_upgrade).await?;
    let v32_cut_data = v32_upgrade_contract.diamond_cut_data(vec![]);

    upgrade_tester
        .set_new_version_on_ctm(v32_cut_data.clone(), deadline, v32_upgrade.newProtocolVersion)
        .await?;
    upgrade_tester
        .set_upgrade_timestamp(v32_upgrade.newProtocolVersion, upgrade_timestamp)
        .await?;
    tracing::info!(
        v32_version = ?v32_upgrade.newProtocolVersion,
        "v32.0 upgrade registered on CTM with timestamp"
    );

    upgrade_tester
        .wait_for_upgrade(v32_upgrade_contract.upgrade_tx_l2_hash())
        .await?;
    tracing::info!("v32.0 upgrade tx confirmed on L2, executing L1 upgrade");

    upgrade_tester.upgrade_chain(v32_cut_data).await?;
    upgrade_tester
        .wait_for_upgrade_finalization(v32_upgrade_contract.upgrade_tx_l2_hash())
        .await?;
    tracing::info!("v32.0 upgrade (with correct SampleForceDeployment) fully finalized");

    // Verify that the CORRECT SampleForceDeployment bytecode is now installed at the target
    // address (the second upgrade must have overwritten the gibberish from the first).
    let force_deployed_contract = SampleForceDeployment::new(
        sample_force_deployment_address,
        upgrade_tester.tester.l2_provider.clone(),
    );
    let stored_value = force_deployed_contract.return42().call().await?;
    assert_eq!(stored_value, U256::from(42));

    Ok(())
}
