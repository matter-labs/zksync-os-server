pub mod models;

use crate::IBridgehub::{
    IBridgehubInstance, L2TransactionRequestDirect, requestL2TransactionDirectCall,
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

        function requestL2TransactionDirect(
            L2TransactionRequestDirect calldata _request
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
        address public validatorTimelock;
    }

    // `IZKChain.sol`
    #[sol(rpc)]
    interface IZKChain {
        function storedBatchHash(uint256 _batchNumber) external view returns (bytes32);
        function getTotalBatchesCommitted() external view returns (uint256);
        function getTotalBatchesVerified() external view returns (uint256);
        function getTotalBatchesExecuted() external view returns (uint256);
        function getTotalPriorityTxs() external view returns (uint256);
    }

    // Taken from `IExecutor.sol`
    interface IExecutor {
        struct StoredBatchInfo {
            uint64 batchNumber;
            bytes32 batchHash;
            uint64 indexRepeatedStorageChanges;
            uint256 numberOfLayer1Txs;
            bytes32 priorityOperationsHash;
            bytes32 l2LogsTreeRoot;
            uint256 timestamp;
            bytes32 commitment;
        }

        struct CommitBoojumOSBatchInfo {
            uint64 batchNumber;
            bytes32 newStateCommitment;
            uint256 numberOfLayer1Txs;
            bytes32 priorityOperationsHash;
            bytes32 l2LogsTreeRoot;
            address l2DaValidator;
            bytes32 daCommitment;
            uint64 firstBlockTimestamp;
            uint64 lastBlockTimestamp;
            uint256 chainId;
            bytes operatorDAInput;
        }

        function commitBatchesSharedBridge(
            uint256 _chainId,
            uint256 _processFrom,
            uint256 _processTo,
            bytes calldata _commitData
        ) external;

       function proofPayload(StoredBatchInfo old, StoredBatchInfo[] newInfo, uint256[] proof);

       function proveBatchesSharedBridge(
            uint256, // always zero (used to be chain id)
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
           uint256, // always zero (used to be chain id)
           uint256 _processFrom,
           uint256 _processTo,
           bytes calldata _executeData
       );
    }
}

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
        chain_type_manager.validatorTimelock().call().await
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
        l2_gas_limit: U256,
        l2_gas_per_pubdata_byte_limit: U256,
        refund_recipient: Address,
    ) -> SolCallBuilder<&P, requestL2TransactionDirectCall> {
        self.instance
            .requestL2TransactionDirect(L2TransactionRequestDirect {
                chainId: U256::try_from(self.l2_chain_id).unwrap(),
                mintValue: mint_value,
                l2Contract: l2_contract,
                l2Value: l2_value,
                l2Calldata: l2_calldata.into(),
                l2GasLimit: l2_gas_limit,
                l2GasPerPubdataByteLimit: l2_gas_per_pubdata_byte_limit,
                factoryDeps: vec![],
                refundRecipient: refund_recipient,
            })
    }

    pub async fn l2_transaction_base_cost(
        &self,
        gas_price: U256,
        l2_gas_limit: U256,
        l2_gas_per_pubdata_byte_limit: U256,
    ) -> alloy::contract::Result<U256> {
        self.instance
            .l2TransactionBaseCost(
                U256::try_from(self.l2_chain_id).unwrap(),
                gas_price,
                l2_gas_limit,
                l2_gas_per_pubdata_byte_limit,
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

    pub async fn get_total_batches_committed(&self) -> alloy::contract::Result<U256> {
        self.instance.getTotalBatchesCommitted().call().await
    }

    pub async fn get_total_batches_proved(&self) -> alloy::contract::Result<U256> {
        self.instance.getTotalBatchesVerified().call().await
    }

    pub async fn get_total_batches_executed(&self) -> alloy::contract::Result<U256> {
        self.instance.getTotalBatchesExecuted().call().await
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
