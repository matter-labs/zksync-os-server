pub mod calldata;
pub mod l1_discovery;
mod metrics;
pub mod models;

use crate::IBridgehub::{
    IBridgehubInstance, L2TransactionRequestDirect, L2TransactionRequestTwoBridgesOuter,
    requestL2TransactionDirectCall, requestL2TransactionTwoBridgesCall,
};
use crate::IMultisigCommitter::IMultisigCommitterInstance;
use crate::IZKChain::IZKChainInstance;
use alloy::contract::SolCallBuilder;
use alloy::eips::BlockId;
use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, TxHash, U256};
use alloy::providers::Provider;

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
    struct InteropRoot {
        uint256 chainId;
        uint256 blockOrBatchNumber;
        bytes32[] sides;
    }

    // `IMessageRoot.sol`
    #[sol(rpc)]
    interface IMessageRoot {
        // Event that is being emitted by GW
        event NewInteropRoot (
            uint256 indexed chainId,
            uint256 indexed blockNumber,
            uint256 indexed logId,
            bytes32[] sides
        );

        // Event that is being emmited by L1
        event AppendedChainRoot(uint256 indexed chainId, uint256 indexed batchNumber, bytes32 indexed chainRoot);

        function addInteropRoot (
            uint256 chainId,
            uint256 blockOrBatchNumber,
            bytes32[] calldata sides
        );

        function addInteropRootsInBatch(InteropRoot[] calldata interopRootsInput);
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
        function getL2SystemContractsUpgradeTxHash() external view returns (bytes32);
        function getL2SystemContractsUpgradeBatchNumber() external view returns (uint256);
        function baseTokenGasPriceMultiplierNominator() external view returns (uint128);
        function baseTokenGasPriceMultiplierDenominator() external view returns (uint128);
        function getBaseToken() external view returns (address);
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

    // `BytecodeSupplier.sol`
    interface IBytecodeSupplier {
        event BytecodePublished(bytes32 indexed bytecodeHash, bytes bytecode);
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
        let zk_chain_address = self
            .instance
            .getZKChain(U256::from(self.l2_chain_id))
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
            Err(e) if e.as_revert_data().is_some() => Ok(None),
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

    pub async fn stored_batch_hash(&self, batch_number: u64) -> Result<B256> {
        self.instance
            .storedBatchHash(U256::from(batch_number))
            .call()
            .await
            .enrich("storedBatchHash", None)
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

    /// Returns current upgrade transaction waiting to be executed. Zeroed out if not present.
    pub async fn get_upgrade_tx_hash(&self, block_id: BlockId) -> Result<TxHash> {
        self.instance
            .getL2SystemContractsUpgradeTxHash()
            .block(block_id)
            .call()
            .await
            .enrich("getL2SystemContractsUpgradeTxHash", Some(block_id))
    }

    /// Returns batch number that contains current upgrade transaction. Returns `0` if not present.
    pub async fn get_upgrade_batch_number(&self, block_id: BlockId) -> Result<u64> {
        self.instance
            .getL2SystemContractsUpgradeBatchNumber()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
            .enrich("getL2SystemContractsUpgradeBatchNumber", Some(block_id))
    }

    /// Returns base token address.
    pub async fn get_base_token_address(&self) -> Result<Address> {
        self.instance
            .getBaseToken()
            .call()
            .await
            .enrich("getBaseToken", None)
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
