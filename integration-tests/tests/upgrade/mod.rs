use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::ext::AnvilApi;
use alloy::providers::{PendingTransactionBuilder, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use std::collections::BTreeMap;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;
use zksync_os_integration_tests::provider::ZksyncApi as _;

use self::interfaces::*;

mod interfaces;

// We assume that chain has this ID.
const CHAIN_ID: u64 = 270;

/// All the data we need to fetch about the contracts configuration.
#[derive(Debug)]
struct ContractsConfiguration {
    // Bridgehub contract
    bridgehub: Bridgehub::BridgehubInstance<EthDynProvider>,
    // Bridgehub owner address
    bridgehub_owner: Address,
    // CTM contract
    ctm: ChainTypeManager::ChainTypeManagerInstance<EthDynProvider>,
    // CTM owner address
    ctm_owner: Address,
    // L1 chain admin contract
    l1_chain_admin: ChainAdmin::ChainAdminInstance<EthDynProvider>,
    // L1 chain admin owner address
    l1_chain_admin_owner: Address,
    // Diamond proxy on the settlement layer
    diamond_proxy: ZkChain::ZkChainInstance<EthDynProvider>,
    // Diamond proxy owner address
    diamond_proxy_admin: Address,
    // Current protocol version
    protocol_version: U256,
    // TODO (not discoverable without config right now): // Address of bytecode supplier contract
    // l1_bytecode_supplier: Address,
}

impl ContractsConfiguration {
    // Fetch the contracts configuration from the tester.
    async fn fetch(tester: &Tester) -> anyhow::Result<Self> {
        let bridgehub = tester.l2_zk_provider.get_bridgehub_contract().await?;
        let bridgehub = Bridgehub::new(bridgehub, tester.l1_provider.clone());
        let ctm = bridgehub
            .chainTypeManager(U256::from(CHAIN_ID))
            .call()
            .await?;
        let ctm = ChainTypeManager::new(ctm, tester.l1_provider.clone());
        let protocol_version = ctm.getProtocolVersion(U256::from(CHAIN_ID)).call().await?;

        let diamond_proxy = bridgehub.getZKChain(U256::from(CHAIN_ID)).call().await?;
        let diamond_proxy = ZkChain::new(diamond_proxy, tester.l1_provider.clone());

        let l1_chain_admin = diamond_proxy.getAdmin().call().await?;
        let l1_chain_admin = ChainAdmin::new(l1_chain_admin, tester.l1_provider.clone());

        let bridgehub_owner = bridgehub.owner().call().await?;
        let ctm_owner = ctm.owner().call().await?;
        let diamond_proxy_admin = diamond_proxy.getAdmin().call().await?;
        let l1_chain_admin_owner = l1_chain_admin.owner().call().await?;

        Ok(Self {
            bridgehub,
            bridgehub_owner,
            ctm,
            ctm_owner,
            diamond_proxy,
            diamond_proxy_admin,
            l1_chain_admin,
            l1_chain_admin_owner,
            protocol_version,
        })
    }
}

/// This test assumes that governance is an EOA account, and uses impersonation
/// to execute the upgrade with it.
/// It also assumes that we don't have a gateway.
#[test_log::test(tokio::test)]
async fn upgrade_with_force_deployment() -> anyhow::Result<()> {
    let upgrade_timestamp = U256::from(0);

    // Test that we can deposit L2 funds from a rich L1 account
    let tester = Tester::setup().await?;

    let config = ContractsConfiguration::fetch(&tester).await?;
    // Enable impersonation and fund all governance accounts
    for addr in [
        config.bridgehub_owner,
        config.ctm_owner,
        config.diamond_proxy_admin,
        config.l1_chain_admin_owner,
    ] {
        tester.l1_provider.anvil_impersonate_account(addr).await?;
        tester
            .l1_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(addr)
                    .with_value(U256::from(10).pow(U256::from(18u64))), // 1 ETH
            )
            .await?
            .expect_successful_receipt()
            .await?;
    }

    tester
        .l1_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(config.bridgehub_owner)
                .with_value(U256::from(10).pow(U256::from(18u64))), // 1 ETH
        )
        .await?
        .expect_successful_receipt()
        .await?;

    let upgrade_contract = DefaultUpgrade::deploy(tester.l1_provider.clone()).await?;

    // Send pause migration to Bridgehub

    let pause_migration_tx = config
        .bridgehub
        .pauseMigration()
        .into_transaction_request()
        .with_from(config.bridgehub_owner);
    let hash = tester
        .l1_provider
        .anvil_send_impersonated_transaction(pause_migration_tx)
        .await?;
    PendingTransactionBuilder::new(tester.l1_provider.root().clone(), hash)
        .expect_successful_receipt()
        .await?;

    // HACK: right now we need to call an account with bytecode to make the upgrade work.
    // So we deploy a event emitter contract and use it as a delegate.
    let event_emitter =
        zksync_os_integration_tests::contracts::EventEmitter::deploy(tester.l2_provider.clone())
            .await?;
    let event_emitter_calldata = event_emitter
        .emitEvent(U256::from(42u64))
        .calldata()
        .clone();

    // Prepare protocol upgrade
    let semver_minor_version_multiplier = U256::from(4294967296u64);
    let new_protocol_version =
        config.protocol_version + semver_minor_version_multiplier + U256::from(1u64); // Bump minor and patch.
    let protocol_upgrade = ProtocolUpgradeBuilder::new(
        new_protocol_version,
        (*event_emitter.address(), event_emitter_calldata),
    )
    .with_force_deployments(BTreeMap::new())
    .with_timestamp(upgrade_timestamp)
    .build();
    let init_calldata = upgrade_contract
        .upgrade(protocol_upgrade)
        .calldata()
        .clone();

    // STM upgrade, `setNewVersionUpgrade` call;
    let upgrade_data = DiamondCutData {
        facetCuts: vec![],
        initAddress: *upgrade_contract.address(),
        initCalldata: init_calldata,
    };
    let deadline = U256::MAX; // The protocol version will not have any deadline in this upgrade
    let tx = config
        .ctm
        .setNewVersionUpgrade(
            upgrade_data.clone(),
            config.protocol_version,
            deadline,
            new_protocol_version,
        )
        .into_transaction_request()
        .with_from(config.ctm_owner);
    let hash = tester
        .l1_provider
        .anvil_send_impersonated_transaction(tx)
        .await?;
    PendingTransactionBuilder::new(tester.l1_provider.root().clone(), hash)
        .expect_successful_receipt()
        .await?;

    // Set timestamp for upgrade on a specific chain under stm, `setUpgradeTimestamp` call on L1ChainAdmin
    let tx = config
        .l1_chain_admin
        .setUpgradeTimestamp(new_protocol_version, upgrade_timestamp)
        .into_transaction_request()
        .with_from(config.l1_chain_admin_owner);
    let hash = tester
        .l1_provider
        .anvil_send_impersonated_transaction(tx)
        .await?;
    PendingTransactionBuilder::new(tester.l1_provider.root().clone(), hash)
        .expect_successful_receipt()
        .await?;

    // Check that the server executed the upgrade

    // Chain upgrade, `upgradeChainFromVersion` call
    let tx = config
        .diamond_proxy
        .upgradeChainFromVersion(config.protocol_version, upgrade_data)
        .into_transaction_request()
        .with_from(config.diamond_proxy_admin);
    let hash = tester
        .l1_provider
        .anvil_send_impersonated_transaction(tx)
        .await?;
    PendingTransactionBuilder::new(tester.l1_provider.root().clone(), hash)
        .expect_successful_receipt()
        .await?;

    // Check that new batches are committed

    Ok(())
}

#[derive(Debug)]
pub struct ProtocolUpgradeBuilder {
    /// New protocol version
    protocol_version: U256,

    /// List of contracts to be force-deployed during the upgrade.
    force_deployments: Option<BTreeMap<Address, Bytes>>,
    /// Address and calldata to delegate the upgrade logic to.
    /// MUST correspond to an account with bytecode, tx will revert if code on
    /// account will be empty.
    /// If you don't need to execute any logic during the upgrade, deploy an
    /// empty contract and use its address here.
    /// TODO: make it an `Option` once the contracts are fixed
    delegate_to: (Address, Bytes),
    /// Timestamp after which upgrade can be executed
    /// If not provided, default value will be used (e.g. upgrade whenever)
    timestamp: U256,
    // I don't need more parameters here for now, but in the future if you want
    // to extend this builder, feel free to do it.
}

impl ProtocolUpgradeBuilder {
    /// Create a new `ProtocolUpgradeBuilder` with default values.
    pub fn new(protocol_version: U256, delegate_to: (Address, Bytes)) -> Self {
        Self {
            protocol_version,
            force_deployments: None,
            delegate_to,
            timestamp: U256::ZERO,
        }
    }

    /// Optional. Sets the list of contracts to be force-deployed during the upgrade.
    pub fn with_force_deployments(mut self, deployments: BTreeMap<Address, Bytes>) -> Self {
        self.force_deployments = Some(deployments);
        self
    }

    /// Sets the timestamp after which the upgrade can be executed.
    pub fn with_timestamp(mut self, timestamp: U256) -> Self {
        self.timestamp = timestamp;
        self
    }

    /// Builds the `ProposedUpgrade` struct.
    pub fn build(self) -> ProposedUpgrade {
        let force_deployments = self.force_deployments.unwrap_or_default();
        let (delegate_to_address, delegate_to_calldata) = self.delegate_to;

        const DEPLOYER_ADDRESS: &str = "0x0000000000000000000000000000000000008007";
        const COMPLEX_UPGRADER_ADDRESS: &str = "0x000000000000000000000000000000000000800f";

        const REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_LIMIT: u32 = 800;

        let force_deployments = force_deployments
            .into_iter()
            .map(|(address, bytecode)| UniversalForceDeploymentInfo {
                isZKsyncOS: true,
                deployedBytecodeInfo: bytecode,
                newAddress: address,
            })
            .collect::<Vec<UniversalForceDeploymentInfo>>();

        let data = L2ComplexUpgrader::forceDeployAndUpgradeUniversalCall {
            _forceDeployments: force_deployments,
            _delegateTo: delegate_to_address,
            _calldata: delegate_to_calldata,
        }
        .abi_encode();

        let l2_upgrade_tx = L2CanonicalTransaction {
            txType: U256::from(126), // TODO: Check if correct, could be 254 if old code
            from: DEPLOYER_ADDRESS.parse().unwrap(),
            to: COMPLEX_UPGRADER_ADDRESS.parse().unwrap(),
            gasLimit: U256::from(72000000u64), // TODO: value copy-pasted from Era upgrade test
            gasPerPubdataByteLimit: U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_LIMIT),
            maxFeePerGas: U256::from(0),
            maxPriorityFeePerGas: U256::from(0),
            paymaster: U256::from(0),
            nonce: U256::from(0),
            value: U256::from(0),
            reserved: [U256::from(0); 4],
            data: Bytes::from(data),
            signature: Bytes::default(),
            factoryDeps: Vec::new(),
            paymasterInput: Bytes::default(),
            reservedDynamic: Bytes::default(),
        };

        let verifier_params = VerifierParams {
            recursionCircuitsSetVksHash: Default::default(),
            recursionLeafLevelVkHash: Default::default(),
            recursionNodeLevelVkHash: Default::default(),
        };

        ProposedUpgrade {
            l2ProtocolUpgradeTx: l2_upgrade_tx,
            bootloaderHash: Default::default(),
            defaultAccountHash: Default::default(),
            evmEmulatorHash: Default::default(),
            verifier: Default::default(),
            verifierParams: verifier_params,
            l1ContractsUpgradeCalldata: Default::default(),
            postUpgradeCalldata: Default::default(),
            upgradeTimestamp: self.timestamp,
            newProtocolVersion: self.protocol_version,
        }
    }
}
