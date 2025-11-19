use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol_types::{SolCall, SolValue};
use std::collections::BTreeMap;
use zksync_os_types::{L1TxType as _, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE};
use zksync_os_types::{ProtocolSemanticVersion, UpgradeTxType};

use zk_os_api::helpers::set_properties_code;
use zk_os_basic_system::system_implementation::flat_storage_model::AccountProperties;

use super::interfaces::*;

#[derive(Debug)]
pub struct ProtocolUpgradeBuilder {
    /// Current protocol version (used to determine if the upgrade is patch-only)
    current_protocol_version: ProtocolSemanticVersion,
    /// New protocol version
    protocol_version: ProtocolSemanticVersion,

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
    pub(super) fn new(
        current_protocol_version: ProtocolSemanticVersion,
        delegate_to: (Address, Bytes),
    ) -> Self {
        Self {
            current_protocol_version: current_protocol_version.clone(),
            protocol_version: current_protocol_version,
            force_deployments: None,
            delegate_to,
            timestamp: U256::ZERO,
        }
    }

    /// Sets the delegation target for L2 upgrade logic.
    /// Right now, MUST correspond to a real deployed account with bytecode.
    /// At the time of writing, the upgrade logic MUST be executed during the upgrade,
    /// so this field is mandatory, even if we don't need to execute any specific logic.
    /// By default, `UpgradeTester::protocol_upgrade_builder` will deploy a no-op contract
    /// and set it as the delegate, so this method is only needed if you want to
    /// execute some specific logic during the upgrade.
    pub fn delegate_to(mut self, delegate_address: Address, calldata: Bytes) -> Self {
        self.delegate_to = (delegate_address, calldata);
        self
    }

    /// Bump the minor protocol version by the specified amount.
    pub fn bump_minor(mut self, by: u64) -> Self {
        self.protocol_version = ProtocolSemanticVersion::new(
            0,
            self.protocol_version.minor + by,
            self.protocol_version.patch,
        );
        self
    }

    /// Bump the patch protocol version by the specified amount.
    pub fn bump_patch(mut self, by: u64) -> Self {
        self.protocol_version = ProtocolSemanticVersion::new(
            0,
            self.protocol_version.minor,
            self.protocol_version.patch + by,
        );
        self
    }

    /// Sets the protocol version to the specified value.
    pub fn set_version(mut self, version: ProtocolSemanticVersion) -> Self {
        self.protocol_version = version;
        self
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
        let (delegate_to_address, delegate_to_calldata) = self.delegate_to;

        // Encoded as strings, because they are used as `U256` in the transaction.
        const FORCE_DEPLOYER_ADDRESS: &str = "0x0000000000000000000000000000000000008007";
        const COMPLEX_UPGRADER_ADDRESS: &str = "0x000000000000000000000000000000000000800f";

        let patch_only = self.protocol_version.minor == self.current_protocol_version.minor;

        let mut force_deployments: Vec<UniversalContractUpgradeInfo> = Vec::new();
        let factory_deps = Vec::new();

        for (address, bytecode) in self.force_deployments.unwrap_or_default() {
            let mut account_properties = AccountProperties::default();
            set_properties_code(&mut account_properties, &bytecode);

            // TODO: the current implementation is faulty, since it uses `full_bytecode_len`; the reason for that
            // is the fact that we deploy preimages using creation bytecode (which includes bytecode artifacts),
            // but for "real" force deployments we need to use the actual deployed bytecode.
            // Once BytecodesSupplier is ready for zksync-os, we need to change this logic to use observable bytecode len.
            let deployed_bytecode_info = super::interfaces::ForceDeploymentBytecodeInfo {
                bytecodeHash: B256::from_slice(account_properties.bytecode_hash.as_u8_ref()),
                bytecodeSize: account_properties.full_bytecode_len(),
                observableBytecodeHash: B256::from_slice(
                    account_properties.observable_bytecode_hash.as_u8_ref(),
                ),
            };
            force_deployments.push(UniversalContractUpgradeInfo {
                upgradeType: ContractUpgradeType::ZKsyncOSUnsafeForceDeployment,
                deployedBytecodeInfo: deployed_bytecode_info.abi_encode().into(),
                newAddress: address,
            });

            // TODO: with current version of bytecodes supplier, we cannot really publish EVM bytecodes
            // factory_deps.push(U256::from_be_slice(
            //     deployed_bytecode_info.bytecodeHash.as_ref(),
            // ));
        }

        let tx_type = if patch_only {
            assert!(
                force_deployments.is_empty(),
                "patch-only upgrades cannot have force deployments"
            );
            assert!(
                factory_deps.is_empty(),
                "patch-only upgrades cannot have factory deps"
            );
            // Patch upgrades do not have a dedicated transaction, so associated transaction must be ignored.
            U256::from(0)
        } else {
            U256::from(UpgradeTxType::TX_TYPE)
        };

        let data = L2ComplexUpgrader::forceDeployAndUpgradeUniversalCall {
            _forceDeployments: force_deployments,
            _delegateTo: delegate_to_address,
            _calldata: delegate_to_calldata,
        }
        .abi_encode();

        let l2_upgrade_tx = L2CanonicalTransaction {
            txType: tx_type,
            from: FORCE_DEPLOYER_ADDRESS.parse().unwrap(),
            to: COMPLEX_UPGRADER_ADDRESS.parse().unwrap(),
            gasLimit: U256::from(72000000u64), // Value copied from Era, it has to be this big due to pubdata costs.
            gasPerPubdataByteLimit: U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE),
            maxFeePerGas: U256::from(0),
            maxPriorityFeePerGas: U256::from(0),
            paymaster: U256::from(0),
            nonce: U256::from(self.protocol_version.minor), // Nonce must correspond to minor version per contract rules.
            value: U256::from(0),
            reserved: [U256::from(0); 4],
            data: Bytes::from(data),
            signature: Bytes::default(),
            factoryDeps: factory_deps, // Contains hashes of all force-deployed bytecodes
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
            newProtocolVersion: self
                .protocol_version
                .packed()
                .expect("incorrect protocol version"),
        }
    }
}
