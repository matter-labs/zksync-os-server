use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use std::collections::BTreeMap;
use zksync_os_integration_tests::contracts::SampleForceDeployment;
use zksync_os_integration_tests::upgrade::{
    Action, CommitterFacetV32, ExecutorFacetV32, FacetCut, UpgradeTester,
};
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester};

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
    let upgrade_tester = UpgradeTester::for_default_upgrade(&tester).await?;

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

/// Performs V31->V32 protocol upgrade with a force deployment, exercising the legacy path
/// where the node already knows the bytecode preimage from a prior L2 deployment.
#[test_log::test(tokio::test)]
#[ignore = "needs the v32 ecosystem contracts upgrade (MessageRoot.addChainBatchRootV32 is missing on a v31-deployed L1); chain facet cuts alone cannot model it"]
async fn upgrade_to_v32_with_predeployed_bytecodes() -> anyhow::Result<()> {
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

    let tester = Tester::setup().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(&tester).await?;

    // Pre-register the force-deployment bytecode via an L2 create tx.
    // This test exercises the legacy path where the node already knows the preimage
    // and the upgrade tx does not carry `factory_deps`.
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

    // The post-upgrade server speaks the v32 wire, so the settlement facets must be v32.
    let l1_chain_id = upgrade_tester.tester.l1_provider().get_chain_id().await?;
    let committer_facet = CommitterFacetV32::deploy(
        upgrade_tester.tester.l1_provider().clone(),
        U256::from(l1_chain_id),
    )
    .await?;
    let executor_facet =
        ExecutorFacetV32::deploy(upgrade_tester.tester.l1_provider().clone()).await?;

    let facet_cuts = vec![
        FacetCut {
            facet: *committer_facet.address(),
            action: Action::Replace,
            isFreezable: true,
            selectors: vec![FixedBytes(
                CommitterFacetV32::commitBatchesSharedBridgeCall::SELECTOR,
            )],
        },
        FacetCut {
            facet: *executor_facet.address(),
            action: Action::Replace,
            isFreezable: true,
            selectors: vec![
                FixedBytes(ExecutorFacetV32::proveBatchesSharedBridgeCall::SELECTOR),
                FixedBytes(ExecutorFacetV32::executeBatchesSharedBridgeCall::SELECTOR),
                FixedBytes(ExecutorFacetV32::revertBatchesSharedBridgeCall::SELECTOR),
            ],
        },
    ];

    upgrade_tester
        .execute_default_upgrade(
            &protocol_upgrade,
            deadline,
            upgrade_timestamp,
            false,
            facet_cuts,
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
    let en1 = upgrade_tester
        .tester
        .launch_from_config(upgrade_tester.tester.external_node_config())
        .await?;

    while en1.l2_provider.get_block_number().await? < main_node_block {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Ok(())
}

/// Performs V31->V32 protocol upgrade which also does a force deployment.
#[test_log::test(tokio::test)]
#[ignore = "needs the v32 ecosystem contracts upgrade (MessageRoot.addChainBatchRootV32 is missing on a v31-deployed L1); chain facet cuts alone cannot model it"]
async fn upgrade_to_v32_with_deployments() -> anyhow::Result<()> {
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

    let tester = CURRENT_TO_L1.environment().await?.launch_default().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(&tester).await?;

    // Publish the raw runtime bytecode from the force-deployment payload to the
    // L1 BytecodesSupplier. This exercises the supplier-backed path where the
    // node discovers force-deployment preimages from `EVMBytecodePublished`
    // events using the upgrade tx `factory_deps`.
    upgrade_tester
        .publish_bytecodes_to_l1_supplier([SampleForceDeployment::DEPLOYED_BYTECODE.clone()])
        .await?;

    // Prepare protocol upgrade with `factory_deps` so the node fetches preimages
    // from the supplier instead of relying on a prior L2 deployment.
    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .bump_minor(1)
        .with_force_deployments(force_deployments)
        .with_factory_deps()
        .with_timestamp(upgrade_timestamp)
        .build();

    // Same v32 facet cuts as `upgrade_to_v32_with_predeployed_bytecodes`.
    let l1_chain_id = upgrade_tester.tester.l1_provider().get_chain_id().await?;
    let committer_facet = CommitterFacetV32::deploy(
        upgrade_tester.tester.l1_provider().clone(),
        U256::from(l1_chain_id),
    )
    .await?;
    let executor_facet =
        ExecutorFacetV32::deploy(upgrade_tester.tester.l1_provider().clone()).await?;
    let facet_cuts = vec![
        FacetCut {
            facet: *committer_facet.address(),
            action: Action::Replace,
            isFreezable: true,
            selectors: vec![FixedBytes(
                CommitterFacetV32::commitBatchesSharedBridgeCall::SELECTOR,
            )],
        },
        FacetCut {
            facet: *executor_facet.address(),
            action: Action::Replace,
            isFreezable: true,
            selectors: vec![
                FixedBytes(ExecutorFacetV32::proveBatchesSharedBridgeCall::SELECTOR),
                FixedBytes(ExecutorFacetV32::executeBatchesSharedBridgeCall::SELECTOR),
                FixedBytes(ExecutorFacetV32::revertBatchesSharedBridgeCall::SELECTOR),
            ],
        },
    ];

    upgrade_tester
        .execute_default_upgrade(
            &protocol_upgrade,
            deadline,
            upgrade_timestamp,
            false,
            facet_cuts,
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
    let en1 = upgrade_tester
        .tester
        .launch_from_config(upgrade_tester.tester.external_node_config())
        .await?;

    while en1.l2_provider.get_block_number().await? < main_node_block {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Ok(())
}
