pub mod calldata;
pub mod l1_discovery;
mod metrics;
pub mod models;

use crate::IBridgehub::{
    IBridgehubInstance, L2TransactionRequestDirect, L2TransactionRequestTwoBridgesOuter,
    requestL2TransactionDirectCall, requestL2TransactionTwoBridgesCall,
};
use crate::IMessageRoot::IMessageRootInstance;
use crate::IMultisigCommitter::IMultisigCommitterInstance;
use crate::IZKChain::IZKChainInstance;
use alloy::contract::SolCallBuilder;
use alloy::eips::BlockId;
use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use zksync_os_provider::NodeProvider;

alloy::sol! {
    // `Messaging.sol`
    struct L2CanonicalTransaction {
        uint256 txType;
        uint256 from;
        uint256 to;
        uint256 gasLimit;
        uint256 gasPerPubdataByteLimit;
        uint256 maxFeePerGas;
        uint256 maxPriorityFeePerGas;
        uint256 paymaster;
        uint256 nonce;
        uint256 value;
        uint256[4] reserved;
        bytes data;
        bytes signature;
        uint256[] factoryDeps;
        bytes paymasterInput;
        bytes reservedDynamic;
    }

    // `Messaging.sol`
    #[derive(Debug)]
    struct InteropRoot {
        uint256 chainId;
        uint256 blockOrBatchNumber;
        bytes32[] sides;
    }

    interface ServerNotifier {
        event UpgradeTimestampUpdated(uint256 indexed chainId, uint256 indexed protocolVersion, uint256 upgradeTimestamp);
    }

    interface ISystemContext {
        function setSettlementLayerChainId(uint256 _newSettlementLayerChainId);
    }

    interface IInteropCenter {
        function setInteropFee(uint256 _interopFee);
        function interopProtocolFee() external view returns (uint256);
    }

    #[sol(rpc)]
    interface IL2InteropCommitmentTree {
        struct IMTLeaf {
            uint256 value;
            uint256 nextIndex;
            uint256 nextValue;
        }

        function leafCount() external view returns (uint256);
        function leafAt(uint256 index) external view returns (IMTLeaf memory);
    }

    #[sol(rpc)]
    interface IGWAssetTracker {
        function gatewaySettlementFee() external view returns (uint256);
    }

    // `DynamicIncrementalMerkle.sol`
    struct Bytes32PushTree {
        uint256 _nextLeafIndex;
        bytes32[] _sides;
        bytes32[] _zeros;
    }

    // `IMessageRoot.sol`
    #[sol(rpc)]
    interface IMessageRoot {
        // Emitted whenever MessageRoot advances the shared interop root imported by chains.
        event NewInteropRoot (
            uint256 indexed chainId,
            uint256 indexed blockNumber,
            uint256 indexed logId,
            bytes32[] sides
        );

        // Emitted when a chain root is appended to the shared tree.
        event AppendedChainRoot(uint256 indexed chainId, uint256 indexed batchNumber, bytes32 indexed chainRoot);

        function addInteropRoot (
            uint256 chainId,
            uint256 blockOrBatchNumber,
            bytes32[] calldata sides
        );

        function addInteropRootsInBatch(InteropRoot[] calldata interopRootsInput);

        uint256 public interopRootLogId;

        function getChainTree(uint256 chainId) public view returns (Bytes32PushTree);

        // `l1Timestamp` is part of the batch-leaf preimage, so proofs bind the batch to L1 time.
        event AppendedChainBatchRoot(uint256 indexed chainId, uint256 indexed batchNumber, bytes32 chainBatchRoot, uint256 l1Timestamp);
        function getMerklePathForChain(uint256 _chainId) external view returns (bytes32[] memory);
        mapping(uint256 chainId => uint256 chainIndex) public chainIndex;
    }

    // `ZKChainStorage.sol`
    enum PubdataPricingMode {
        Rollup,
        Validium
    }

    // `IMailbox.sol`
    interface IMailbox {
        event NewPriorityRequest(
            uint256 txId,
            bytes32 txHash,
            uint64 expirationTimestamp,
            L2CanonicalTransaction transaction,
            bytes[] factoryDeps
        );
    }

    // `IBridgehub.sol`
    #[sol(rpc)]
    interface IBridgehub {
        function getZKChain(uint256 _chainId) external view returns (address);
        function chainTypeManager(uint256 _chainId) external view returns (address);
        function sharedBridge() public view returns (address);
        function getAllZKChainChainIDs() external view returns (uint256[] memory);
        function messageRoot() external view returns (address);
        function whitelistedSettlementLayers(uint256 _chainId) external view returns (bool);
        function chainAssetHandler() external view returns (address);

        struct L2TransactionRequestDirect {
            uint256 chainId;
            uint256 mintValue;
            address l2Contract;
            uint256 l2Value;
            bytes l2Calldata;
            uint256 l2GasLimit;
            uint256 l2GasPerPubdataByteLimit;
            bytes[] factoryDeps;
            address refundRecipient;
        }

        struct L2TransactionRequestTwoBridgesOuter {
            uint256 chainId;
            uint256 mintValue;
            uint256 l2Value;
            uint256 l2GasLimit;
            uint256 l2GasPerPubdataByteLimit;
            address refundRecipient;
            address secondBridgeAddress;
            uint256 secondBridgeValue;
            bytes secondBridgeCalldata;
        }

        function requestL2TransactionDirect(
            L2TransactionRequestDirect calldata _request
        ) external payable returns (bytes32 canonicalTxHash);

        function requestL2TransactionTwoBridges(
            L2TransactionRequestTwoBridgesOuter calldata _request
        ) external payable returns (bytes32 canonicalTxHash);

        function l2TransactionBaseCost(
            uint256 _chainId,
            uint256 _gasPrice,
            uint256 _l2GasLimit,
            uint256 _l2GasPerPubdataByteLimit
        ) external view returns (uint256);
    }

    // `IChainTypeManager.sol`
    #[sol(rpc)]
    interface IChainTypeManager {
        address public validatorTimelockPostV29;

        function serverNotifierAddress() external view returns (address);

        enum Action {
            Add,
            Replace,
            Remove
        }

        struct FacetCut {
            address facet;
            Action action;
            bool isFreezable;
            bytes4[] selectors;
        }

        struct DiamondCutData {
            FacetCut[] facetCuts;
            address initAddress;
            bytes initCalldata;
        }

        struct VerifierParams {
            bytes32 recursionNodeLevelVkHash;
            bytes32 recursionLeafLevelVkHash;
            bytes32 recursionCircuitsSetVksHash;
        }

        struct ProposedUpgrade {
            L2CanonicalTransaction l2ProtocolUpgradeTx;
            bytes32 bootloaderHash;
            bytes32 defaultAccountHash;
            bytes32 evmEmulatorHash;
            address verifier;
            VerifierParams verifierParams;
            bytes l1ContractsUpgradeCalldata;
            bytes postUpgradeCalldata;
            uint256 upgradeTimestamp;
            uint256 newProtocolVersion;
        }

        /// Defines an upgrade from version A to version B
        event NewProtocolVersion(uint256 indexed oldProtocolVersion, uint256 indexed newProtocolVersion);

        /// Provides an actual data for the upgrade execution.
        event NewUpgradeCutData(uint256 indexed protocolVersion, DiamondCutData diamondCutData);

        /// Address of the L1 bytecodes supplier used for upgrades (v31+).
        function L1_BYTECODES_SUPPLIER() external view returns (address);

        /// The block number on the CTM's chain where `setUpgradeDiamondCutInner` ran for the
        /// given (old) protocol version. Non-zero means this CTM owns the upgrade cut data for
        /// that version. Populated starting with the V31 ChainTypeManager.
        function upgradeCutDataBlock(uint256 protocolVersion) external view returns (uint256);
    }

    // `ValidatorTimelock.sol`
    // Used by the node startup flow to revert committed batches before local block rebuild.
    #[sol(rpc)]
    interface IValidatorTimelock {
        function REVERTER_ROLE() external view returns (bytes32);
        function hasRoleForChainId(uint256 _chainId, bytes32 _role, address _address) external view returns (bool);
        function revertBatchesSharedBridge(address _chainAddress, uint256 _newLastBatch) external;
    }

    // `SettlementLayerV31UpgradeBase.sol` — the per-chain upgrade init contract.
    // `NewUpgradeCutData` carries a placeholder `additionalForceDeploymentsData`
    // that `upgradeChainFromVersion` rewrites per-chain inside the delegatecall
    // via `getL2UpgradeTxData(bridgehub, chainId, existingTxData)`. The server
    // must call this before executing the L2 upgrade tx — otherwise the
    // placeholder's empty `additionalForceDeploymentsData` would revert inside
    // `performForceDeployedContractsInit`.
    #[sol(rpc)]
    interface ISettlementLayerV31Upgrade {
        function getL2UpgradeTxData(
            address _bridgehub,
            uint256 _chainId,
            bool _zksyncOS,
            bytes memory _existingTxData
        ) external view returns (bytes memory);
    }

    // `IZKChain.sol`
    #[sol(rpc)]
    interface IZKChain {
        function storedBatchHash(uint256 _batchNumber) external view returns (bytes32);
        function getTotalBatchesCommitted() external view returns (uint256);
        function getTotalBatchesVerified() external view returns (uint256);
        function getTotalBatchesExecuted() external view returns (uint256);
        function getTotalPriorityTxs() external view returns (uint256);
        function getPubdataPricingMode() external view returns (PubdataPricingMode);
        function getAdmin() external view returns (address);
        function getChainTypeManager() external view returns (address);
        function getProtocolVersion() external view returns (uint256);
        function baseTokenGasPriceMultiplierNominator() external view returns (uint128);
        function baseTokenGasPriceMultiplierDenominator() external view returns (uint128);
        function getBaseToken() external view returns (address);
        function getSettlementLayer() external view returns (address);
    }

    // Taken from `common/Config.sol`
    enum L2DACommitmentScheme {
        NONE,
        EMPTY_NO_DA,
        PUBDATA_KECCAK256,
        BLOBS_AND_PUBDATA_KECCAK256,
        BLOBS_ZKSYNC_OS
    }

    // Taken from `IExecutor.sol`
    interface IExecutor {
        struct StoredBatchInfo {
            uint64 batchNumber;
            bytes32 batchHash;
            uint64 indexRepeatedStorageChanges;
            uint256 numberOfLayer1Txs;
            bytes32 priorityOperationsHash;
            bytes32 dependencyRootsRollingHash;
            bytes32 l2LogsTreeRoot;
            uint256 timestamp;
            bytes32 commitment;
        }

        struct CommitBatchInfoZKsyncOS {
            uint64 batchNumber;
            bytes32 newStateCommitment;
            uint256 numberOfLayer1Txs;
            uint256 numberOfLayer2Txs;
            bytes32 priorityOperationsHash;
            bytes32 dependencyRootsRollingHash;
            bytes32 l2LogsTreeRoot;
            L2DACommitmentScheme daCommitmentScheme;
            bytes32 daCommitment;
            uint64 firstBlockTimestamp;
            uint64 firstBlockNumber;
            uint64 lastBlockTimestamp;
            uint64 lastBlockNumber;
            uint256 chainId;
            bytes operatorDAInput;
            uint256 slChainId;
        }

        event BlockCommit(uint256 indexed batchNumber, bytes32 indexed batchHash, bytes32 indexed commitment);
        event BlockExecution(uint256 indexed batchNumber, bytes32 indexed batchHash, bytes32 indexed commitment);
        #[derive(Debug)]
        event ReportCommittedBatchRangeZKsyncOS(
            uint64 indexed batchNumber,
            uint64 indexed firstBlockNumber,
            uint64 indexed lastBlockNumber
        );
        #[derive(Debug)]
        event BlocksRevert(uint256 totalBatchesCommitted, uint256 totalBatchesVerified, uint256 totalBatchesExecuted);

        function commitBatchesSharedBridge(
            address _chainAddress,
            uint256 _processFrom,
            uint256 _processTo,
            bytes calldata _commitData
        ) external;

        function proofPayload(StoredBatchInfo old, StoredBatchInfo[] newInfo, uint256[] proof);

        function proveBatchesSharedBridge(
            address _chainAddress,
            uint256 _processBatchFrom,
            uint256 _processBatchTo,
            bytes calldata _proofData
        );

        struct PriorityOpsBatchInfo {
            bytes32[] leftPath;
            bytes32[] rightPath;
            bytes32[] itemHashes;
        }

        struct L2Log {
           uint8 l2ShardId;
           bool isService;
           uint16 txNumberInBatch;
           address sender;
           bytes32 key;
           bytes32 value;
       }

        function executeBatchesSharedBridge(
            address _chainAddress,
            uint256 _processFrom,
            uint256 _processTo,
            bytes calldata _executeData
        );
    }

    // taken from v29 version of `IExecutor.sol`
    // We need this to make the server work with the v29 version of contracts during the upgrade, and it can be removed after
    interface IExecutorV29 {
        struct CommitBatchInfoZKsyncOS {
            uint64 batchNumber;
            bytes32 newStateCommitment;
            uint256 numberOfLayer1Txs;
            bytes32 priorityOperationsHash;
            bytes32 dependencyRootsRollingHash;
            bytes32 l2LogsTreeRoot;
            address l2DaValidator;
            bytes32 daCommitment;
            uint64 firstBlockTimestamp;
            uint64 lastBlockTimestamp;
            uint256 chainId;
            bytes operatorDAInput;
        }
    }

    // taken from v30 version of `IExecutor.sol`
    // This format is still required to submit v30 batches before the upgrade to v31.
    interface IExecutorV30 {
        struct CommitBatchInfoZKsyncOS {
            uint64 batchNumber;
            bytes32 newStateCommitment;
            uint256 numberOfLayer1Txs;
            bytes32 priorityOperationsHash;
            bytes32 dependencyRootsRollingHash;
            bytes32 l2LogsTreeRoot;
            L2DACommitmentScheme daCommitmentScheme;
            bytes32 daCommitment;
            uint64 firstBlockTimestamp;
            uint64 firstBlockNumber;
            uint64 lastBlockTimestamp;
            uint64 lastBlockNumber;
            uint256 chainId;
            bytes operatorDAInput;
        }
    }

    // `IL1GenesisUpgrade.sol`
    interface IL1GenesisUpgrade {
        event GenesisUpgrade(
            address indexed _zkChain,
            L2CanonicalTransaction _l2Transaction,
            uint256 indexed _protocolVersion,
            bytes[] _factoryDeps
        );
    }

    // `IChainAdmin.sol`
    interface IChainAdmin {
        event UpdateUpgradeTimestamp(uint256 indexed protocolVersion, uint256 upgradeTimestamp);
    }

    // `IChainAdminOwnable.sol`
    #[sol(rpc)]
    interface IChainAdminOwnable {
        function setTokenMultiplier(address _chainContract, uint128 _nominator, uint128 _denominator) external;
        // Not present in `IChainAdminOwnable`, but `ChainAdminOwnable` which is the only implementor has it.
        function tokenMultiplierSetter() external view returns (address);
    }

    // `BytecodesSupplier.sol`
    interface IBytecodeSupplier {
        event EVMBytecodePublished(bytes32 indexed bytecodeHash, bytes bytecode);
    }

    #[sol(rpc)]
    interface IMultisigCommitter {

        function commitBatchesMultisig(
            address chainAddress,
            uint256 _processBatchFrom,
            uint256 _processBatchTo,
            bytes calldata _batchData,
            address[] calldata signers,
            bytes[] calldata signatures
        ) external;

        function getSigningThreshold(address chainAddress) external view returns (uint64);

        function isValidator(address chainAddress, address validator) external view returns (bool);

        function getValidatorsCount(address chainAddress) external view returns (uint256);

        function getValidatorsMember(address chainAddress, uint256 index) external view returns (address);
    }

    #[sol(rpc)]
    interface IERC20 {
        function decimals() external view returns (uint8);
    }
}

pub struct MessageRoot<P: Provider> {
    instance: IMessageRootInstance<P, Ethereum>,
    address: Address,
}

impl<P: Provider> MessageRoot<P> {
    pub fn new(address: Address, provider: P) -> Self {
        let instance = IMessageRoot::new(address, provider);
        Self { instance, address }
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    pub fn provider(&self) -> &P {
        self.instance.provider()
    }

    pub async fn interop_root_log_id(&self, block_id: BlockId) -> Result<u64> {
        self.instance
            .interopRootLogId()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
            .enrich("interopRootLogId", Some(block_id))
    }
}

impl MessageRoot<NodeProvider> {
    /// L1 block at which this message root contract was deployed, used as the lower bound for
    /// binary searches over L1 history. Convenience over [`NodeProvider::deployment_block`] that
    /// the provider caches per address.
    pub async fn deployment_block(&self) -> anyhow::Result<u64> {
        self.provider().deployment_block(self.address).await
    }
}

#[derive(Clone, Debug)]
pub struct Bridgehub<P: Provider> {
    instance: IBridgehubInstance<P, Ethereum>,
    l2_chain_id: u64,
}

impl<P: Provider + Clone> Bridgehub<P> {
    pub fn new(address: Address, provider: P, l2_chain_id: u64) -> Self {
        let instance = IBridgehub::new(address, provider);
        Self {
            instance,
            l2_chain_id,
        }
    }

    pub fn address(&self) -> &Address {
        self.instance.address()
    }

    pub fn provider(&self) -> &P {
        self.instance.provider()
    }

    pub async fn message_root_address(&self) -> alloy::contract::Result<Address> {
        self.instance.messageRoot().call().await
    }

    pub async fn chain_type_manager_address(&self) -> alloy::contract::Result<Address> {
        self.instance
            .chainTypeManager(U256::from(self.l2_chain_id))
            .call()
            .await
    }

    // TODO: Consider creating a separate `ChainTypeManager` struct
    pub async fn validator_timelock_address(&self) -> alloy::contract::Result<Address> {
        let chain_type_manager_address = self.chain_type_manager_address().await?;
        let chain_type_manager =
            IChainTypeManager::new(chain_type_manager_address, self.instance.provider());
        chain_type_manager.validatorTimelockPostV29().call().await
    }

    pub async fn shared_bridge_address(&self) -> alloy::contract::Result<Address> {
        self.instance.sharedBridge().call().await
    }

    #[allow(clippy::too_many_arguments)]
    pub fn request_l2_transaction_direct(
        &self,
        mint_value: U256,
        l2_contract: Address,
        l2_value: U256,
        l2_calldata: Vec<u8>,
        l2_gas_limit: u64,
        l2_gas_per_pubdata_byte_limit: u64,
        refund_recipient: Address,
    ) -> SolCallBuilder<&P, requestL2TransactionDirectCall> {
        self.instance
            .requestL2TransactionDirect(L2TransactionRequestDirect {
                chainId: U256::try_from(self.l2_chain_id).unwrap(),
                mintValue: mint_value,
                l2Contract: l2_contract,
                l2Value: l2_value,
                l2Calldata: l2_calldata.into(),
                l2GasLimit: U256::from(l2_gas_limit),
                l2GasPerPubdataByteLimit: U256::from(l2_gas_per_pubdata_byte_limit),
                factoryDeps: vec![],
                refundRecipient: refund_recipient,
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn request_l2_transaction_two_bridges(
        &self,
        mint_value: U256,
        l2_value: U256,
        l2_gas_limit: u64,
        l2_gas_per_pubdata_byte_limit: u64,
        refund_recipient: Address,
        second_bridge_address: Address,
        second_bridge_value: U256,
        second_bridge_calldata: Vec<u8>,
    ) -> SolCallBuilder<&P, requestL2TransactionTwoBridgesCall> {
        self.instance
            .requestL2TransactionTwoBridges(L2TransactionRequestTwoBridgesOuter {
                chainId: U256::try_from(self.l2_chain_id).unwrap(),
                mintValue: mint_value,
                l2Value: l2_value,
                l2GasLimit: U256::from(l2_gas_limit),
                l2GasPerPubdataByteLimit: U256::from(l2_gas_per_pubdata_byte_limit),
                refundRecipient: refund_recipient,
                secondBridgeAddress: second_bridge_address,
                secondBridgeValue: second_bridge_value,
                secondBridgeCalldata: second_bridge_calldata.into(),
            })
    }

    pub async fn l2_transaction_base_cost(
        &self,
        gas_price: u128,
        l2_gas_limit: u64,
        l2_gas_per_pubdata_byte_limit: u64,
    ) -> alloy::contract::Result<U256> {
        self.instance
            .l2TransactionBaseCost(
                U256::from(self.l2_chain_id),
                U256::from(gas_price),
                U256::from(l2_gas_limit),
                U256::from(l2_gas_per_pubdata_byte_limit),
            )
            .call()
            .await
    }

    pub async fn zk_chain(&self) -> alloy::contract::Result<ZkChain<P>> {
        self.zk_chain_by_chain_id(self.l2_chain_id).await
    }

    pub async fn zk_chain_by_chain_id(&self, chain_id: u64) -> alloy::contract::Result<ZkChain<P>> {
        let zk_chain_address = self
            .instance
            .getZKChain(U256::from(chain_id))
            .call()
            .await?;
        Ok(ZkChain::new(
            zk_chain_address,
            self.instance.provider().clone(),
        ))
    }

    pub async fn get_all_zk_chain_chain_ids(&self) -> alloy::contract::Result<Vec<U256>> {
        self.instance.getAllZKChainChainIDs().call().await
    }
}

#[derive(Clone, Debug)]
pub struct MultisigCommitter<P: Provider> {
    instance: IMultisigCommitterInstance<P, Ethereum>,
    chain_address: Address,
}

impl<P: Provider> MultisigCommitter<P> {
    pub fn new(address: Address, provider: P, chain_address: Address) -> Self {
        let instance = IMultisigCommitter::new(address, provider);
        Self {
            instance,
            chain_address,
        }
    }

    /// Checks if the contract at the given address implements the `IMultisigCommitter` interface
    /// by calling `getSigningThreshold`. Returns `Some(Self)` if successful, `None` if the call
    /// reverts (indicating the contract doesn't implement the interface), or an error for other
    /// failures (e.g., network errors).
    pub async fn try_new(
        address: Address,
        provider: P,
        chain_address: Address,
    ) -> core::result::Result<Option<Self>, alloy::contract::Error> {
        let instance = IMultisigCommitter::new(address, provider);
        let result = instance.getSigningThreshold(chain_address).call().await;
        match result {
            Ok(_) => Ok(Some(Self {
                instance,
                chain_address,
            })),
            Err(e) if e.to_string().contains("revert") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn get_signing_threshold(&self) -> Result<u64> {
        self.instance
            .getSigningThreshold(self.chain_address)
            .call()
            .await
            .enrich("getSigningThreshold", None)
    }

    pub async fn is_validator(&self, validator: Address) -> Result<bool> {
        self.instance
            .isValidator(self.chain_address, validator)
            .call()
            .await
            .enrich("isValidator", None)
    }

    pub async fn get_validators_count(&self) -> Result<U256> {
        self.instance
            .getValidatorsCount(self.chain_address)
            .call()
            .await
            .enrich("getValidatorsCount", None)
    }

    pub async fn get_validator(&self, index: U256) -> Result<Address> {
        self.instance
            .getValidatorsMember(self.chain_address, index)
            .call()
            .await
            .enrich("getValidatorsMember", None)
    }

    /// Returns the list of all validators for the chain.
    pub async fn get_validators(&self) -> Result<Vec<Address>> {
        let count = self.get_validators_count().await?;
        let count: u64 = count.saturating_to();
        let mut validators = Vec::with_capacity(count as usize);
        for i in 0..count {
            let validator = self.get_validator(U256::from(i)).await?;
            validators.push(validator);
        }
        Ok(validators)
    }
}

#[derive(Clone, Debug)]
pub struct ZkChain<P: Provider> {
    instance: IZKChainInstance<P, Ethereum>,
}

impl ZkChain<NodeProvider> {
    /// L1 block at which this diamond proxy was deployed, used as the lower bound for binary
    /// searches over L1 history. Convenience over [`NodeProvider::deployment_block`] that the
    /// provider caches per address.
    pub async fn deployment_block(&self) -> anyhow::Result<u64> {
        self.provider().deployment_block(*self.address()).await
    }
}

impl<P: Provider> ZkChain<P> {
    pub fn new(address: Address, provider: P) -> Self {
        let instance = IZKChainInstance::new(address, provider);
        Self { instance }
    }

    pub fn address(&self) -> &Address {
        self.instance.address()
    }

    pub fn provider(&self) -> &P {
        self.instance.provider()
    }

    pub async fn stored_batch_hash(&self, batch_number: u64, block_id: BlockId) -> Result<B256> {
        self.instance
            .storedBatchHash(U256::from(batch_number))
            .block(block_id)
            .call()
            .await
            .enrich("storedBatchHash", Some(block_id))
    }

    pub async fn get_total_batches_committed(&self, block_id: BlockId) -> Result<u64> {
        self.instance
            .getTotalBatchesCommitted()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
            .enrich("getTotalBatchesCommitted", Some(block_id))
    }

    pub async fn get_total_batches_proved(&self, block_id: BlockId) -> Result<u64> {
        self.instance
            .getTotalBatchesVerified()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
            .enrich("getTotalBatchesVerified", Some(block_id))
    }

    pub async fn get_total_batches_executed(&self, block_id: BlockId) -> Result<u64> {
        self.instance
            .getTotalBatchesExecuted()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
            .enrich("getTotalBatchesExecuted", Some(block_id))
    }

    pub async fn get_total_priority_txs_at_block(&self, block_id: BlockId) -> Result<u64> {
        self.instance
            .getTotalPriorityTxs()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
            .enrich("getTotalPriorityTxs", Some(block_id))
    }

    pub async fn get_pubdata_pricing_mode(&self) -> Result<PubdataPricingMode> {
        self.instance
            .getPubdataPricingMode()
            .call()
            .await
            .enrich("getPubdataPricingMode", None)
    }

    /// Returns true iff the contract has non-empty code at `block_id`.
    pub async fn code_exists_at_block(&self, block_id: BlockId) -> alloy::contract::Result<bool> {
        let code = self
            .provider()
            .get_code_at(*self.address())
            .block_id(block_id)
            .await?;

        Ok(!code.0.is_empty())
    }

    /// Returns the current admin of the chain.
    pub async fn get_admin(&self) -> Result<Address> {
        self.instance
            .getAdmin()
            .call()
            .await
            .enrich("getAdmin", None)
    }

    /// Returns the current CTM for the chain.
    pub async fn get_chain_type_manager(&self) -> Result<Address> {
        self.instance
            .getChainTypeManager()
            .call()
            .await
            .enrich("getChainTypeManager", None)
    }

    /// Returns the current protocol version of the chain.
    /// Returned value is the raw (U256) representation.
    pub async fn get_raw_protocol_version(&self, block_id: BlockId) -> Result<U256> {
        self.instance
            .getProtocolVersion()
            .block(block_id)
            .call()
            .await
            .enrich("getProtocolVersion", Some(block_id))
    }

    /// Returns base token address.
    pub async fn get_base_token_address(&self) -> Result<Address> {
        self.instance
            .getBaseToken()
            .call()
            .await
            .enrich("getBaseToken", None)
    }

    /// Returns base token gas price multiplier nominator.
    pub async fn base_token_gas_price_multiplier_nominator(&self) -> Result<u128> {
        self.instance
            .baseTokenGasPriceMultiplierNominator()
            .call()
            .await
            .enrich("baseTokenGasPriceMultiplierNominator", None)
    }

    /// Returns base token gas price multiplier denominator.
    pub async fn base_token_gas_price_multiplier_denominator(&self) -> Result<u128> {
        self.instance
            .baseTokenGasPriceMultiplierDenominator()
            .call()
            .await
            .enrich("baseTokenGasPriceMultiplierDenominator", None)
    }

    /// Returns the address of the settlement layer as stored in `ZKChainStorage` at `block_id`.
    pub async fn get_settlement_layer(&self, block_id: BlockId) -> Result<Address> {
        self.instance
            .getSettlementLayer()
            .block(block_id)
            .call()
            .await
            .enrich("getSettlementLayer", Some(block_id))
    }

    pub async fn get_server_notifier_address(&self) -> Result<Address> {
        let chain_type_manager = self.get_chain_type_manager().await?;
        let chain_type_manager_instance =
            IChainTypeManager::new(chain_type_manager, self.provider());
        chain_type_manager_instance
            .serverNotifierAddress()
            .call()
            .await
            .enrich("serverNotifierAddress", None)
    }
}

/// Returns `true` for an empty contract response or empty EVM revert.
pub fn is_method_missing(err: &alloy::contract::Error) -> bool {
    match err {
        alloy::contract::Error::ZeroData(..) => true,
        alloy::contract::Error::TransportError(te) => {
            let Some(response) = te.as_error_resp() else {
                return false;
            };
            // Geth-compatible RPCs report EVM execution reverts with code 3.
            if response.code != 3 || !response.message.to_ascii_lowercase().contains("revert") {
                return false;
            }

            response
                .as_revert_data()
                .is_some_and(|data| data.is_empty())
        }
        _ => false,
    }
}

/// Enriched error when interacting with contracts.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to call `{1}`: {0}")]
    Call(Box<alloy::contract::Error>, String),
    #[error("failed to call `{1}` at block id `{2}`: {0}")]
    CallAtBlock(Box<alloy::contract::Error>, String, BlockId),
}

pub type Result<T> = core::result::Result<T, Error>;

trait Enrich {
    type Output;
    fn enrich(self, function_name: &str, block_id: Option<BlockId>) -> Result<Self::Output>;
}

impl<T> Enrich for alloy::contract::Result<T> {
    type Output = T;
    fn enrich(self, function_name: &str, block_id: Option<BlockId>) -> Result<Self::Output> {
        self.map_err(|e| match block_id {
            None => Error::Call(Box::new(e), function_name.to_string()),
            Some(block_id) => Error::CallAtBlock(Box::new(e), function_name.to_string(), block_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::is_method_missing;
    use alloy::rpc::json_rpc::{ErrorPayload, RpcSend};
    use alloy::transports::TransportError;
    use serde::Serialize;

    #[derive(Clone, Debug, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct NestedRevert {
        original_error: RevertData,
    }

    #[derive(Clone, Debug, Serialize)]
    struct RevertData {
        data: &'static str,
    }

    fn rpc_error<T: RpcSend>(
        code: i64,
        message: &'static str,
        data: Option<T>,
    ) -> alloy::contract::Error {
        let payload = ErrorPayload {
            code,
            message: message.into(),
            data,
        }
        .serialize_payload()
        .unwrap();
        alloy::contract::Error::TransportError(TransportError::ErrorResp(payload))
    }

    #[test]
    fn method_missing_requires_an_empty_revert() {
        assert!(is_method_missing(&rpc_error(
            3,
            "execution reverted",
            Some("0x"),
        )));
        assert!(is_method_missing(&rpc_error(
            3,
            "execution reverted",
            Some(NestedRevert {
                original_error: RevertData { data: "0x" },
            }),
        )));
        assert!(!is_method_missing(&rpc_error(
            3,
            "execution reverted",
            Some("0xdeadbeef"),
        )));
        assert!(!is_method_missing(&rpc_error(
            -32603,
            "execution reverted",
            Option::<&str>::None,
        )));
    }
}
