use crate::IExecutor;
use alloy::primitives::{Address, B256, Bytes, Keccak256, U256, keccak256};
use alloy::sol_types::SolValue;
use serde::{Deserialize, Serialize};
use std::fmt;

/// User-friendly version of [`IExecutor::PriorityOpsBatchInfo`].
#[derive(Clone, Debug, Default)]
pub struct PriorityOpsBatchInfo {
    pub left_path: Vec<B256>,
    pub right_path: Vec<B256>,
    pub item_hashes: Vec<B256>,
}

impl From<PriorityOpsBatchInfo> for IExecutor::PriorityOpsBatchInfo {
    fn from(value: PriorityOpsBatchInfo) -> Self {
        IExecutor::PriorityOpsBatchInfo {
            leftPath: value.left_path,
            rightPath: value.right_path,
            itemHashes: value.item_hashes,
        }
    }
}

/// User-friendly version of [`crate::PubdataPricingMode`] with statically known possible variants.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum BatchDaInputMode {
    Rollup,
    Validium,
}

/// User-friendly version of [`IExecutor::StoredBatchInfo`] containing
/// fields that are relevant for ZKsync OS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredBatchInfo {
    pub batch_number: u64,
    pub state_commitment: B256,
    pub number_of_layer1_txs: u64,
    pub priority_operations_hash: B256,
    pub dependency_roots_rolling_hash: B256,
    pub l2_to_l1_logs_root_hash: B256,
    pub commitment: B256,
    pub last_block_timestamp: u64,
}

impl StoredBatchInfo {
    pub fn hash(&self) -> B256 {
        let abi_encoded = IExecutor::StoredBatchInfo::from(self).abi_encode_params();
        keccak256(abi_encoded.as_slice())
    }
}

impl From<&StoredBatchInfo> for IExecutor::StoredBatchInfo {
    fn from(value: &StoredBatchInfo) -> Self {
        Self::from((
            // `batchNumber`
            value.batch_number,
            // `batchHash` - for ZKsync OS batches we store full state commitment here
            value.state_commitment,
            // `indexRepeatedStorageChanges` - Not used in Boojum OS, must be zero
            0u64,
            // `numberOfLayer1Txs`
            U256::from(value.number_of_layer1_txs),
            // `priorityOperationsHash`
            value.priority_operations_hash,
            // `dependencyRootsRollingHash`,
            value.dependency_roots_rolling_hash,
            // `l2LogsTreeRoot`
            value.l2_to_l1_logs_root_hash,
            // `timestamp` - Not used in ZKsync OS, must be zero
            U256::from(0),
            // `commitment` - For ZKsync OS batches we store batch output hash here
            value.commitment,
        ))
    }
}

/// User-friendly version of [`IExecutor::CommitBatchInfoZKsyncOS`].
#[derive(Clone, Serialize, Deserialize)]
pub struct CommitBatchInfo {
    pub batch_number: u64,
    pub new_state_commitment: B256,
    pub number_of_layer1_txs: u64,
    pub priority_operations_hash: B256,
    pub dependency_roots_rolling_hash: B256,
    pub l2_to_l1_logs_root_hash: B256,
    pub l2_da_validator: Address,
    pub da_commitment: B256,
    pub first_block_timestamp: u64,
    pub last_block_timestamp: u64,
    pub chain_id: u64,
    pub chain_address: Address,
    pub operator_da_input: Vec<u8>,
    pub upgrade_tx_hash: Option<B256>,
}

impl CommitBatchInfo {
    /// Calculate keccak256 hash of public input
    pub fn public_input_hash(&self) -> B256 {
        let mut hasher = Keccak256::new();
        hasher.update(U256::from(self.chain_id).to_be_bytes::<32>());
        hasher.update(&self.first_block_timestamp.to_be_bytes());
        hasher.update(&self.last_block_timestamp.to_be_bytes());
        hasher.update(&self.l2_da_validator);
        hasher.update(&self.da_commitment);
        hasher.update(U256::from(self.number_of_layer1_txs).to_be_bytes::<32>());
        hasher.update(&self.priority_operations_hash);
        hasher.update(&self.l2_to_l1_logs_root_hash);
        hasher.update(&self.upgrade_tx_hash.unwrap_or_default());
        hasher.update(&self.dependency_roots_rolling_hash);
        hasher.finalize()
    }
}

impl From<CommitBatchInfo> for IExecutor::CommitBatchInfoZKsyncOS {
    fn from(value: CommitBatchInfo) -> Self {
        IExecutor::CommitBatchInfoZKsyncOS::from((
            value.batch_number,
            value.new_state_commitment,
            U256::from(value.number_of_layer1_txs),
            value.priority_operations_hash,
            value.dependency_roots_rolling_hash,
            value.l2_to_l1_logs_root_hash,
            Address::from(value.l2_da_validator.0),
            value.da_commitment,
            value.first_block_timestamp,
            value.last_block_timestamp,
            U256::from(value.chain_id),
            Bytes::from(value.operator_da_input),
        ))
    }
}

impl fmt::Debug for CommitBatchInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommitBatchInfo")
            .field("batch_number", &self.batch_number)
            .field("new_state_commitment", &self.new_state_commitment)
            .field("number_of_layer1_txs", &self.number_of_layer1_txs)
            .field("priority_operations_hash", &self.priority_operations_hash)
            .field(
                "dependency_roots_rolling_hash",
                &self.dependency_roots_rolling_hash,
            )
            .field("l2_to_l1_logs_root_hash", &self.l2_to_l1_logs_root_hash)
            .field("l2_da_validator", &self.l2_da_validator)
            .field("da_commitment", &self.da_commitment)
            .field("first_block_timestamp", &self.first_block_timestamp)
            .field("last_block_timestamp", &self.last_block_timestamp)
            .field("chain_id", &self.chain_id)
            .field("chain_address", &self.chain_address)
            // .field("operator_da_input", skipped to keep concise!)
            .field("upgrade_tx_hash", &self.upgrade_tx_hash)
            .finish()
    }
}

impl From<CommitBatchInfo> for StoredBatchInfo {
    fn from(value: CommitBatchInfo) -> Self {
        let commitment = value.public_input_hash();
        Self {
            batch_number: value.batch_number,
            state_commitment: value.new_state_commitment,
            number_of_layer1_txs: value.number_of_layer1_txs,
            priority_operations_hash: value.priority_operations_hash,
            dependency_roots_rolling_hash: value.dependency_roots_rolling_hash,
            l2_to_l1_logs_root_hash: value.l2_to_l1_logs_root_hash,
            commitment,
            last_block_timestamp: value.last_block_timestamp,
        }
    }
}
