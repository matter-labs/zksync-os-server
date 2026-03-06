use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use std::collections::BTreeMap;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::contracts::SampleForceDeployment;
use zksync_os_integration_tests::upgrade::{
    Action, CommitterFacetV31, DefaultUpgrade, FacetCut, UpgradeTester,
};

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

/// Tests that the l1_watcher correctly handles a chain that skips an intermediate version.
///
/// Scenario: chain is at v30.X. Two upgrade cut-data entries are published on the CTM:
///   - v30.X+1 (patch, no L2 upgrade tx) — only the cut data is registered, no timestamp.
///   - v31     (minor, has L2 upgrade tx) — cut data registered AND timestamp set.
///
/// The chain receives only the v31 `UpdateUpgradeTimestamp` event, so
/// `fetch_intermediate_upgrade_infos` must detect the v30.X+1 cut data and insert it into
/// the `UpgradeSubpool` (with `tx: None`) before the v31 entry. The sequencer then processes
/// v30.X+1 as a patch-only step (no block tx) and v31 as a minor upgrade (upgrade tx in
/// block), yielding a final chain version of v31.
#[test_log::test(tokio::test)]
async fn upgrade_skip_intermediate_patch_then_minor() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(0); // Upgrade can be executed immediately.
    let deadline = U256::MAX; // No deadline for old protocol version.

    let tester = Tester::setup().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;

    // Build the intermediate patch upgrade (e.g., v30.0 → v30.1).
    // We register the cut data on the CTM so the watcher can detect it as an
    // intermediate version, but we intentionally withhold the `setUpgradeTimestamp`
    // call for this chain — the chain will skip straight to v31.
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

    // Bridgehub migrations must be paused before any setNewVersionUpgrade call.
    upgrade_tester.pause_bridgehub_migrations().await?;

    // Register the intermediate patch upgrade on the CTM.
    // This emits NewUpgradeCutData(v30.1, patchCutData), which the watcher will later find
    // when scanning for intermediate versions between the chain's current and target versions.
    // No setUpgradeTimestamp is called for v30.1 — this chain skips it.
    upgrade_tester
        .set_new_version_on_ctm(
            intermediate_cut_data,
            deadline,
            intermediate_upgrade.newProtocolVersion,
        )
        .await?;
    tracing::info!(
        intermediate_version = ?intermediate_upgrade.newProtocolVersion,
        "Intermediate patch upgrade registered on CTM (no timestamp set)"
    );

    // Build the final minor upgrade (e.g., v30.0 → v31).
    // The chain is still at v30.0, so we pass v30.0 as the old version to the CTM.
    // This second setNewVersionUpgrade call overwrites upgradeCutHash[v30.0] with the
    // minor cut data hash, which is what upgradeChainFromVersion will verify against.
    let final_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_minor(1)
        .with_force_deployments(BTreeMap::new())
        .with_timestamp(upgrade_timestamp)
        .build();

    let final_upgrade_contract =
        DefaultUpgrade::deploy(upgrade_tester.tester.l1_provider(), &final_upgrade).await?;
    let final_cut_data = final_upgrade_contract.diamond_cut_data(vec![]);

    // Register the final minor upgrade on the CTM and set the upgrade timestamp.
    // NewUpgradeCutData(v31, minorCutData) is emitted. The watcher picks up the
    // UpdateUpgradeTimestamp event, calls fetch_intermediate_upgrade_infos, finds v30.1,
    // and submits both entries to the UpgradeSubpool in ascending order.
    upgrade_tester
        .set_new_version_on_ctm(
            final_cut_data.clone(),
            deadline,
            final_upgrade.newProtocolVersion,
        )
        .await?;
    upgrade_tester
        .set_upgrade_timestamp(final_upgrade.newProtocolVersion, upgrade_timestamp)
        .await?;
    tracing::info!(
        final_version = ?final_upgrade.newProtocolVersion,
        "Final minor upgrade registered on CTM with timestamp"
    );

    // Wait for the v31 upgrade tx to land on L2.
    // The sequencer first processes v30.1 (patch-only, no tx in block) then v31 (upgrade tx).
    upgrade_tester
        .wait_for_upgrade(final_upgrade_contract.upgrade_tx_l2_hash())
        .await?;
    tracing::info!("Final minor upgrade tx confirmed on L2, executing L1 upgrade");

    // Finalize the chain upgrade on L1: upgrades from v30.0 to v31.
    // upgradeChainFromVersion verifies hash(finalCutData) == CTM.upgradeCutHash[v30.0],
    // which holds because the second setNewVersionUpgrade overwrote it.
    upgrade_tester.upgrade_chain(final_cut_data).await?;

    upgrade_tester
        .wait_for_upgrade_finalization(final_upgrade_contract.upgrade_tx_l2_hash())
        .await?;
    tracing::info!("Upgrade to v31 via skipped intermediate v30.X+1 fully finalized");

    Ok(())
}
