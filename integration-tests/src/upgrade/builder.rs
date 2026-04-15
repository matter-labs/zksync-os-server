use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol_types::{SolCall, SolValue};
use blake2::{Blake2s256, Digest};
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
    /// Whether to include bytecode hashes in `factory_deps` of the upgrade tx.
    /// When true, the server's `fetch_force_preimages` will look up bytecodes
    /// from the L1 `BytecodesSupplier`. When false, the server relies on
    /// preimages already known from L2 deploys.
    ///
    /// TODO: Remove once v30 support is dropped — `BytecodesSupplier` will be
    /// the only path and this flag should always be true.
    include_factory_deps: bool,
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
            include_factory_deps: false,
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

    /// Includes bytecode hashes in the upgrade tx's `factory_deps`, which causes the
    /// server to fetch preimages from the L1 `BytecodesSupplier` via `EVMBytecodePublished` events.
    pub fn with_factory_deps(mut self) -> Self {
        self.include_factory_deps = true;
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
        let mut factory_deps = Vec::new();

        for (address, bytecode) in self.force_deployments.unwrap_or_default() {
            // Three distinct hashes appear in this flow — keep them straight:
            //
            //   1) `keccak256(raw_bytecode)`
            //        Aliases: `observable_bytecode_hash`, "Era-style hash".
            //        Used by the EVM `EXTCODEHASH` opcode and indexed by
            //        `BytecodesSupplier.publishEVMBytecode` (`era-contracts/.../
            //        BytecodesSupplier.sol:64` calls `ZKSyncOSBytecodeInfo.
            //        hashEVMBytecodeCalldata` which is `keccak256(_bytecode)`).
            //
            //   2) `Blake2s256(raw_bytecode)`
            //        Aliases: "raw blake hash". This is the value the era-contracts
            //        docstring at `ZKSyncOSBytecodeInfo.sol:30` calls
            //        `_bytecodeBlakeHash` (note: docstring says "Blake2b" but the
            //        VM uses Blake2s). This is the **lookup key** the
            //        `set_bytecode_on_address` system hook on L2 hands to the
            //        preimage cache, and the corresponding preimage body must be
            //        exactly the raw bytecode (length = observable len). It is also
            //        what the upgrade tx's `factory_deps` array carries so the
            //        server can fetch matching preimages from `BytecodesSupplier`.
            //
            //   3) `Blake2s256(raw + padding + artifacts)`
            //        Aliases: `account_properties.bytecode_hash`, "padded blake
            //        hash". This is what `set_properties_code` (in
            //        `zksync-os/api/src/helpers.rs`) computes and stores on the
            //        account *after* the system hook recomputes EVM artifacts
            //        in-VM. Subsequent zksync-os bytecode lookups for this account
            //        go through this hash. **It is NOT the right value to send in
            //        an upgrade tx** for the BytecodesSupplier path — the supplier
            //        publishes raw bytes, not padded+artifacts, so this hash never
            //        keys anything in the supplier-fed preimage cache.
            //
            // Two preimage shapes also appear:
            //
            //   A) Raw bytecode (length = `observable_bytecode_len`)
            //        Published to `BytecodesSupplier`; expected by the system hook
            //        when given hash (2).
            //
            //   B) Raw + padding + artifacts (length = `full_bytecode_len`)
            //        Computed and stored by zksync-os whenever a contract is
            //        deployed via the EVM (`account_cache.rs:947-972`). Keyed by
            //        hash (3). The legacy `publish_bytecodes` path in this test
            //        suite (which `Create`-deploys the bytecode on L2) ends up
            //        registering shape B under hash (3).
            //
            // In production, era-contracts'
            // `L2GenesisForceDeploymentsHelper.unsafeForceDeployZKsyncOS` decodes
            // `(bytecodeHash, bytecodeLength, observableBytecodeHash)` from the
            // upgrade-tx payload and forwards them **unchanged** to
            // `ZKOSContractDeployer.setBytecodeDetailsEVM`, which forwards them
            // **unchanged** again to the `SET_BYTECODE_ON_ADDRESS_HOOK` system
            // hook (`zksync-os/system_hooks/.../set_bytecode_on_address.rs:178`).
            // The hook then calls `set_bytecode_details` which:
            //   - looks up the preimage by `code_hash = bytecodeHash` with
            //     `expected_preimage_len_in_bytes = bytecodeLength`,
            //   - asserts iterator length ≤ expected → panic on mismatch (this is
            //     the "Iterator length exceeds expected preimage length" panic),
            //   - re-derives EVM artifacts from the returned raw bytes,
            //   - records a *new* preimage of shape B keyed by hash (3),
            //   - writes hash (3) to `account.bytecode_hash`.
            //
            // So the upgrade tx must carry: hash (2), `bytecodeLength = observable
            // len`, hash (1) — and the supplier (or test fixture) must register
            // shape A under hash (2). Anything else either panics on length, fails
            // the proof-env hash check, or silently writes an unusable account.
            //
            // ----- Branch selection below -----
            //
            // `include_factory_deps == true` — production / BytecodesSupplier path.
            //   The server's `fetch_force_preimages` will scan supplier events and
            //   register shape A under hash (2). Send hash (2) + observable len.
            //
            // `include_factory_deps == false` — legacy path used by v30→v31 tests.
            //   No supplier interaction; instead the test pre-runs `Create` on L2
            //   to register shape B under hash (3). The system hook still runs the
            //   length check, so the upgrade tx must claim `bytecodeLength =
            //   full_bytecode_len` and `bytecodeHash = hash (3)`. The hook then
            //   "recomputes" artifacts on shape B (treating it as raw), which is
            //   technically wrong but harmless: the real entrypoints sit at offset
            //   0 of shape B, so contract calls still execute correctly. This
            //   mismatch is why the branch must die with v30 support.
            //
            // TODO: Remove the legacy branch once v30 support is dropped and
            // `include_factory_deps` is always true.
            let mut account_properties = AccountProperties::default();
            set_properties_code(&mut account_properties, &bytecode);

            let (bytecode_hash, bytecode_size) = if self.include_factory_deps {
                // Hash (2): Blake2s of the raw EVM bytecode bytes.
                let raw_blake = B256::from_slice(Blake2s256::digest(&bytecode).as_slice());
                (raw_blake, account_properties.observable_bytecode_len)
            } else {
                // Hash (3): Blake2s of (raw + padding + artifacts), pulled from
                // `set_properties_code` above. Only valid because shape B was
                // pre-registered by an L2 deploy.
                (
                    B256::from_slice(account_properties.bytecode_hash.as_u8_ref()),
                    account_properties.full_bytecode_len(),
                )
            };
            let deployed_bytecode_info = super::interfaces::ForceDeploymentBytecodeInfo {
                bytecodeHash: bytecode_hash,
                bytecodeSize: bytecode_size,
                // Hash (1): always Keccak256 of the raw bytes — this is what
                // `EXTCODEHASH` returns and is independent of which lookup-hash
                // path we picked above. Stored verbatim on the account as
                // `observable_bytecode_hash`.
                observableBytecodeHash: B256::from_slice(
                    account_properties.observable_bytecode_hash.as_u8_ref(),
                ),
            };
            force_deployments.push(UniversalContractUpgradeInfo {
                upgradeType: ContractUpgradeType::ZKsyncOSUnsafeForceDeployment,
                deployedBytecodeInfo: deployed_bytecode_info.abi_encode().into(),
                newAddress: address,
            });

            if self.include_factory_deps {
                // `factory_deps` carries the *same* hash the supplier-fed preimage
                // cache will be keyed by — i.e. hash (2). The server's
                // `L1UpgradeTxWatcher::fetch_force_preimages` reads this list and
                // resolves each entry against scanned `EVMBytecodePublished`
                // events, computing Blake2s on each event payload to match.
                factory_deps.push(U256::from_be_slice(
                    deployed_bytecode_info.bytecodeHash.as_ref(),
                ));
            }
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
