use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use std::collections::BTreeMap;
use zksync_os_integration_tests::contracts::SampleForceDeployment;
use zksync_os_integration_tests::upgrade::{Action, CommitterFacetV31, FacetCut, UpgradeTester};
use zksync_os_integration_tests::{GatewayTester, Tester};
use zksync_os_server::default_protocol_version::NEXT_PROTOCOL_VERSION;

/// Executes the simplest patch protocol upgrade:
/// - no contracts are deployed
/// - patch version is bumped by 1
/// - upgrade timestamp is 0
/// Importance of this test: unlike minor version upgrades, patch upgrades
/// do not include an upgrade transaction in the block. Hence, we need to ensure that
/// the system can handle patch upgrades correctly.
#[test_log::test(tokio::test)]
async fn upgrade_patch_no_deployments() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(1); // Protocol upgrade can be executed immediately.
    let deadline = U256::MAX; // The protocol version will not have any deadline in this upgrade

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

#[test_log::test(tokio::test)]
async fn upgrade_patch_no_deployments_gateway() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(1); // Protocol upgrade can be executed immediately.
    let deadline = U256::MAX; // The protocol version will not have any deadline in this upgrade

    // Test that we can deposit L2 funds from a rich L1 account
    let gateway_tester = GatewayTester::builder()
        .protocol_version(NEXT_PROTOCOL_VERSION)
        .num_chains(0)
        .build()
        .await?;
    let tester = gateway_tester.into_gateway();
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

#[test_log::test(tokio::test)]
async fn upgrade_patch_no_deployments_settles_to_gateway() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(1);
    let deadline = U256::MAX;

    let gateway_tester = GatewayTester::builder()
        .protocol_version(NEXT_PROTOCOL_VERSION)
        .num_chains(1)
        .build()
        .await?;
    let tester = gateway_tester.into_primary_chain();
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;

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
    let upgrade_timestamp = U256::from(1); // Protocol upgrade can be executed immediately.
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
    let tester = Tester::builder().enable_p2p().build().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;

    // Publish the bytecodes for upgrade beforehand via L2 deploy
    // so that the preimages are known to the node.
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

/// Performs V31->V32 protocol upgrade which also does a force deployment.
/// Upgraded chain settles to gateway.
#[test_log::test(tokio::test)]
async fn upgrade_to_v32_with_deployments_settles_to_gateway() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(1); // Protocol upgrade can be executed immediately.
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

    let gateway_tester = GatewayTester::builder()
        .protocol_version(NEXT_PROTOCOL_VERSION)
        .num_chains(1)
        .enable_chain_p2p()
        .build()
        .await?;
    let tester = gateway_tester.into_primary_chain();
    let upgrade_tester = UpgradeTester::for_default_upgrade(tester).await?;

    // Publish to the L1 BytecodesSupplier only (no L2 deploy). This exercises
    // the end-to-end path: the server discovers preimages from `EVMBytecodePublished`
    // events via `fetch_force_preimages`.
    upgrade_tester
        .publish_bytecodes_to_l1_supplier([SampleForceDeployment::DEPLOYED_BYTECODE.clone()])
        .await?;

    // Prepare protocol upgrade with factory_deps so the server fetches from the supplier.
    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_minor(1)
        .with_force_deployments(force_deployments)
        .with_factory_deps()
        .with_timestamp(upgrade_timestamp)
        .build();

    upgrade_tester
        .execute_default_upgrade(
            &protocol_upgrade,
            deadline,
            upgrade_timestamp,
            false,
            vec![],
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
