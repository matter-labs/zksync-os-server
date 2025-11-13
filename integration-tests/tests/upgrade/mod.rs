use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use std::collections::BTreeMap;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::contracts::SampleForceDeployment;
use zksync_os_integration_tests::upgrade::UpgradeTester;

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
        .execute_default_upgrade(&protocol_upgrade, deadline, upgrade_timestamp, true)
        .await?;

    Ok(())
}

/// Performs a minor protocol upgrade which also does a force deployment.
#[test_log::test(tokio::test)]
async fn upgrade_minor_with_deployments() -> anyhow::Result<()> {
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
    // TODO: we need to use bytecode instead of creation bytecode for now, since under the hood `publish_bytecodes`
    // actually deploys contracts since BytecodesSupplier is not ready for zksync os
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

    upgrade_tester
        .execute_default_upgrade(&protocol_upgrade, deadline, upgrade_timestamp, false)
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
