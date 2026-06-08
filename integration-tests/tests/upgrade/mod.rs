use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::sol_types::SolCall;
use std::collections::BTreeMap;
use std::time::Duration;
use zksync_os_integration_tests::assert_traits::{DEFAULT_TIMEOUT, ReceiptAssert};
use zksync_os_integration_tests::contracts::{EventEmitter, SampleForceDeployment};
use zksync_os_integration_tests::provider::ZksyncTestingProvider;
use zksync_os_integration_tests::upgrade::{Action, CommitterFacetV31, FacetCut, UpgradeTester};
use zksync_os_integration_tests::{GatewayTester, SettlementLayer, TestCase, Tester};
use zksync_os_server::config::Config;
use zksync_os_server::default_protocol_version::{NEXT_PROTOCOL_VERSION, PROTOCOL_VERSION_V31_0};
use zksync_os_types::ProtocolSemanticVersion;

async fn launch_verifier_en(main_node: &Tester) -> anyhow::Result<Tester> {
    let mut config: Config = main_node.external_node_config();
    config.batch_verification_config.client_enabled = true;
    main_node.launch_from_config(config).await
}

async fn wait_for_en_to_catch_up(main_node: &Tester, en: &Tester) -> anyhow::Result<()> {
    let target_block = main_node.l2_provider.get_block_number().await?;
    tokio::time::timeout(DEFAULT_TIMEOUT, async {
        loop {
            if en.l2_provider.get_block_number().await? >= target_block {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await??;
    Ok(())
}

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

#[test_log::test(tokio::test)]
async fn upgrade_patch_no_deployments_gateway() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(1); // Protocol upgrade can be executed immediately.
    let deadline = U256::MAX; // The protocol version will not have any deadline in this upgrade

    let gateway_tester = GatewayTester::builder()
        .protocol_version(NEXT_PROTOCOL_VERSION)
        .num_chains(0)
        .build()
        .await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(gateway_tester.gateway()).await?;

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
    let upgrade_tester = UpgradeTester::for_default_upgrade(gateway_tester.chain(0)).await?;

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
        .build()
        .await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(gateway_tester.chain(0)).await?;

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
    let en1 = upgrade_tester
        .tester
        .launch_from_config(upgrade_tester.tester.external_node_config())
        .await?;

    while en1.l2_provider.get_block_number().await? < main_node_block {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    Ok(())
}

#[test_log::test(tokio::test)]
async fn upgrade_to_v32_1_with_batch_verification_e2e() -> anyhow::Result<()> {
    let target_protocol_version = ProtocolSemanticVersion::new(0, 32, 1);
    let upgrade_timestamp = U256::from(1);
    let deadline = U256::MAX;

    let env = TestCase {
        protocol_version: PROTOCOL_VERSION_V31_0,
        settlement_layer: SettlementLayer::Gateway,
    }
    .environment()
    .await?;
    let tester = env.launch_default().await?;
    let upgrade_tester = UpgradeTester::for_default_upgrade(&tester).await?;

    let protocol_upgrade = upgrade_tester
        .protocol_upgrade_builder()
        .await?
        .set_version(target_protocol_version.clone())
        .with_force_deployments(BTreeMap::new())
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

    let upgraded_protocol_version = ProtocolSemanticVersion::try_from(
        upgrade_tester
            .ctm_sl
            .getProtocolVersion(U256::from(upgrade_tester.chain_id))
            .call()
            .await?,
    )
    .expect("invalid protocol version stored in CTM");
    assert_eq!(upgraded_protocol_version, target_protocol_version);

    let mut verifier_ready_config = tester.config().clone();
    verifier_ready_config.batcher_config.enabled = false;
    verifier_ready_config
        .batch_verification_config
        .server_enabled = true;
    verifier_ready_config.batch_verification_config.threshold = 1;
    let tester = tester.restart_with_config(verifier_ready_config).await?;

    let en = launch_verifier_en(&tester).await?;
    wait_for_en_to_catch_up(&tester, &en).await?;

    let mut post_upgrade_config = tester.config().clone();
    post_upgrade_config.batcher_config.enabled = true;
    let tester = tester.restart_with_config(post_upgrade_config).await?;
    wait_for_en_to_catch_up(&tester, &en).await?;

    let post_upgrade_receipt = EventEmitter::deploy_builder(tester.l2_provider.clone())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    tester
        .l2_zk_provider
        .wait_finalized_with_timeout(post_upgrade_receipt.block_number.unwrap(), DEFAULT_TIMEOUT)
        .await?;

    let tx_hash = post_upgrade_receipt.transaction_hash;
    let mn_receipt = tester.l2_provider.get_transaction_receipt(tx_hash).await?;
    let en_receipt = en.l2_provider.get_transaction_receipt(tx_hash).await?;
    assert_eq!(mn_receipt, en_receipt);

    Ok(())
}
