pub mod l1_discovery;
mod metrics;
pub mod models;

use crate::IBridgehub::{
    IBridgehubInstance, L2TransactionRequestDirect, L2TransactionRequestTwoBridgesOuter,
    requestL2TransactionDirectCall, requestL2TransactionTwoBridgesCall,
};
use crate::IZKChain::IZKChainInstance;
use alloy::contract::SolCallBuilder;
use alloy::eips::BlockId;
use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, U256};
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
            address l2DaValidator;
            bytes32 daCommitment;
            uint64 firstBlockTimestamp;
            uint64 lastBlockTimestamp;
            uint256 chainId;
            bytes operatorDAInput;
        }

        event BlockCommit(uint256 indexed batchNumber, bytes32 indexed batchHash, bytes32 indexed commitment);
        event BlockExecution(uint256 indexed batchNumber, bytes32 indexed batchHash, bytes32 indexed commitment);

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

    // `IL1GenesisUpgrade.sol`
    interface IL1GenesisUpgrade {
        event GenesisUpgrade(
            address indexed _zkChain,
            L2CanonicalTransaction _l2Transaction,
            uint256 indexed _protocolVersion,
            bytes[] _factoryDeps
        );
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

    pub async fn stored_batch_hash(&self, batch_number: u64) -> alloy::contract::Result<B256> {
        self.instance
            .storedBatchHash(U256::from(batch_number))
            .call()
            .await
    }

    pub async fn get_total_batches_committed(
        &self,
        block_id: BlockId,
    ) -> alloy::contract::Result<u64> {
        self.instance
            .getTotalBatchesCommitted()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
    }

    pub async fn get_total_batches_proved(
        &self,
        block_id: BlockId,
    ) -> alloy::contract::Result<u64> {
        self.instance
            .getTotalBatchesVerified()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
    }

    pub async fn get_total_batches_executed(
        &self,
        block_id: BlockId,
    ) -> alloy::contract::Result<u64> {
        self.instance
            .getTotalBatchesExecuted()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
    }

    pub async fn get_total_priority_txs_at_block(
        &self,
        block_id: BlockId,
    ) -> alloy::contract::Result<u64> {
        self.instance
            .getTotalPriorityTxs()
            .block(block_id)
            .call()
            .await
            .map(|n| n.saturating_to())
    }

    pub async fn get_pubdata_pricing_mode(&self) -> alloy::contract::Result<PubdataPricingMode> {
        self.instance.getPubdataPricingMode().call().await
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
}
