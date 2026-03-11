use crate::{IExecutor, IExecutorV29, IExecutorV30};
use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol_types::SolValue;
use serde::{Deserialize, Serialize};
use std::fmt;
use structdiff::Difference;
use structdiff::StructDiff;

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

/// User-friendly version of [`IExecutor::L2Log`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct L2Log {
    pub l2_shard_id: u8,
    pub is_service: bool,
    pub tx_number_in_batch: u16,
    pub sender: Address,
    pub key: B256,
    pub value: B256,
}

impl From<L2Log> for IExecutor::L2Log {
    fn from(value: L2Log) -> Self {
        IExecutor::L2Log {
            l2ShardId: value.l2_shard_id,
            isService: value.is_service,
            txNumberInBatch: value.tx_number_in_batch,
            sender: value.sender,
            key: value.key,
            value: value.value,
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
    // Unused, to remove in the next breaking version.
    pub last_block_timestamp: Option<u64>,
}

impl PartialEq for StoredBatchInfo {
    fn eq(&self, other: &Self) -> bool {
        // skip `last_block_timestamp` check
        self.batch_number == other.batch_number
            && self.state_commitment == other.state_commitment
            && self.number_of_layer1_txs == other.number_of_layer1_txs
            && self.priority_operations_hash == other.priority_operations_hash
            && self.dependency_roots_rolling_hash == other.dependency_roots_rolling_hash
            && self.l2_to_l1_logs_root_hash == other.l2_to_l1_logs_root_hash
            && self.commitment == other.commitment
    }
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

impl From<IExecutor::StoredBatchInfo> for StoredBatchInfo {
    fn from(value: IExecutor::StoredBatchInfo) -> Self {
        Self {
            batch_number: value.batchNumber,
            state_commitment: value.batchHash,
            number_of_layer1_txs: value.numberOfLayer1Txs.to(),
            priority_operations_hash: value.priorityOperationsHash,
            dependency_roots_rolling_hash: value.dependencyRootsRollingHash,
            l2_to_l1_logs_root_hash: value.l2LogsTreeRoot,
            commitment: value.commitment,
            // unused
            last_block_timestamp: Some(0),
        }
    }
}

/// User-friendly version of [`crate::L2DACommitmentScheme`] with statically known possible variants.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[repr(u8)]
pub enum DACommitmentScheme {
    /// Invalid option
    None,
    /// Empty(`0`) commitment, used for validium
    EmptyNoDA,
    /// Keccak of stateDiffHash and keccak(pubdata), used by 3rd party DA solutions
    PubdataKeccak256,
    /// This commitment includes blobs data and pubdata hash, ZKsync OS always outputs empty blobs, and it's used only for calldata with ZKsync OS
    BlobsAndPubdataKeccak256,
    /// Keccak of blob versioned hashes filled with pubdata, used for blobs DA with ZKsync OS
    BlobsZKsyncOS,
}

impl From<DACommitmentScheme> for crate::L2DACommitmentScheme {
    fn from(value: DACommitmentScheme) -> Self {
        match value {
            DACommitmentScheme::None => crate::L2DACommitmentScheme::NONE,
            DACommitmentScheme::EmptyNoDA => crate::L2DACommitmentScheme::EMPTY_NO_DA,
            DACommitmentScheme::PubdataKeccak256 => crate::L2DACommitmentScheme::PUBDATA_KECCAK256,
            DACommitmentScheme::BlobsAndPubdataKeccak256 => {
                crate::L2DACommitmentScheme::BLOBS_AND_PUBDATA_KECCAK256
            }
            DACommitmentScheme::BlobsZKsyncOS => crate::L2DACommitmentScheme::BLOBS_ZKSYNC_OS,
        }
    }
}

impl From<crate::L2DACommitmentScheme> for DACommitmentScheme {
    fn from(value: crate::L2DACommitmentScheme) -> Self {
        match value {
            crate::L2DACommitmentScheme::NONE => DACommitmentScheme::None,
            crate::L2DACommitmentScheme::EMPTY_NO_DA => DACommitmentScheme::EmptyNoDA,
            crate::L2DACommitmentScheme::PUBDATA_KECCAK256 => DACommitmentScheme::PubdataKeccak256,
            crate::L2DACommitmentScheme::BLOBS_AND_PUBDATA_KECCAK256 => {
                DACommitmentScheme::BlobsAndPubdataKeccak256
            }
            crate::L2DACommitmentScheme::BLOBS_ZKSYNC_OS => DACommitmentScheme::BlobsZKsyncOS,
            crate::L2DACommitmentScheme::__Invalid => {
                panic!("Invalid IExecutor::L2DACommitmentScheme from l1")
            }
        }
    }
}

/// User-friendly version of [`IExecutor::CommitBatchInfoZKsyncOS`].
#[derive(Clone, Serialize, Deserialize, PartialEq, Difference)]
#[difference(expose)]
pub struct CommitBatchInfo {
    pub batch_number: u64,
    pub new_state_commitment: B256,
    pub number_of_layer1_txs: u64,
    #[serde(default)]
    pub number_of_layer2_txs: u64,
    pub priority_operations_hash: B256,
    pub dependency_roots_rolling_hash: B256,
    pub l2_to_l1_logs_root_hash: B256,
    #[serde(default = "default_l2_da_commitment_scheme")]
    pub l2_da_commitment_scheme: DACommitmentScheme,
    pub da_commitment: B256,
    pub first_block_timestamp: u64,
    // Note, that pre-zksync-os-v30 batches did not contain this field.
    pub first_block_number: Option<u64>,
    pub last_block_timestamp: u64,
    // Note, that pre-zksync-os-v30 batches did not contain this field.
    pub last_block_number: Option<u64>,
    pub chain_id: u64,
    pub operator_da_input: Vec<u8>,
    #[serde(default)]
    pub sl_chain_id: u64,
}

// `l2_da_commitment_scheme` is not present in storage for old batches, by default we use `BlobsAndPubdataKeccak256`.
// It corresponds to da commitment used in these batches before adding different DACommitmentScheme options
fn default_l2_da_commitment_scheme() -> DACommitmentScheme {
    DACommitmentScheme::BlobsAndPubdataKeccak256
}

impl From<CommitBatchInfo> for IExecutor::CommitBatchInfoZKsyncOS {
    fn from(value: CommitBatchInfo) -> Self {
        IExecutor::CommitBatchInfoZKsyncOS::from((
            value.batch_number,
            value.new_state_commitment,
            U256::from(value.number_of_layer1_txs),
            U256::from(value.number_of_layer2_txs),
            value.priority_operations_hash,
            value.dependency_roots_rolling_hash,
            value.l2_to_l1_logs_root_hash,
            value.l2_da_commitment_scheme.into(),
            value.da_commitment,
            value.first_block_timestamp,
            // It is expected that for all the newly sent batches this field is always present.
            value.first_block_number.unwrap(),
            value.last_block_timestamp,
            // It is expected that for all the newly sent batches this field is always present.
            value.last_block_number.unwrap(),
            U256::from(value.chain_id),
            Bytes::from(value.operator_da_input),
            U256::from(value.sl_chain_id),
        ))
    }
}

impl From<CommitBatchInfo> for IExecutorV29::CommitBatchInfoZKsyncOS {
    fn from(value: CommitBatchInfo) -> Self {
        IExecutorV29::CommitBatchInfoZKsyncOS::from((
            value.batch_number,
            value.new_state_commitment,
            U256::from(value.number_of_layer1_txs),
            value.priority_operations_hash,
            value.dependency_roots_rolling_hash,
            value.l2_to_l1_logs_root_hash,
            // we always set l2 da validator address
            alloy::primitives::Address::ZERO,
            value.da_commitment,
            value.first_block_timestamp,
            value.last_block_timestamp,
            U256::from(value.chain_id),
            Bytes::from(value.operator_da_input),
        ))
    }
}

impl From<CommitBatchInfo> for IExecutorV30::CommitBatchInfoZKsyncOS {
    fn from(value: CommitBatchInfo) -> Self {
        IExecutorV30::CommitBatchInfoZKsyncOS::from((
            value.batch_number,
            value.new_state_commitment,
            U256::from(value.number_of_layer1_txs),
            value.priority_operations_hash,
            value.dependency_roots_rolling_hash,
            value.l2_to_l1_logs_root_hash,
            value.l2_da_commitment_scheme.into(),
            value.da_commitment,
            value.first_block_timestamp,
            // It is expected that for all the newly sent batches this field is always present.
            value.first_block_number.unwrap(),
            value.last_block_timestamp,
            // It is expected that for all the newly sent batches this field is always present.
            value.last_block_number.unwrap(),
            U256::from(value.chain_id),
            Bytes::from(value.operator_da_input),
        ))
    }
}

impl From<IExecutor::CommitBatchInfoZKsyncOS> for CommitBatchInfo {
    fn from(value: IExecutor::CommitBatchInfoZKsyncOS) -> Self {
        Self {
            batch_number: value.batchNumber,
            new_state_commitment: value.newStateCommitment,
            number_of_layer1_txs: value.numberOfLayer1Txs.to::<u64>(),
            number_of_layer2_txs: value.numberOfLayer2Txs.to::<u64>(),
            priority_operations_hash: value.priorityOperationsHash,
            dependency_roots_rolling_hash: value.dependencyRootsRollingHash,
            l2_to_l1_logs_root_hash: value.l2LogsTreeRoot,
            l2_da_commitment_scheme: value.daCommitmentScheme.into(),
            da_commitment: value.daCommitment,
            first_block_timestamp: value.firstBlockTimestamp,
            first_block_number: Some(value.firstBlockNumber),
            last_block_timestamp: value.lastBlockTimestamp,
            last_block_number: Some(value.lastBlockNumber),
            chain_id: value.chainId.to::<u64>(),
            operator_da_input: value.operatorDAInput.as_ref().to_vec(),
            sl_chain_id: value.slChainId.to::<u64>(),
        }
    }
}

impl From<IExecutorV30::CommitBatchInfoZKsyncOS> for CommitBatchInfo {
    fn from(value: IExecutorV30::CommitBatchInfoZKsyncOS) -> Self {
        Self {
            batch_number: value.batchNumber,
            new_state_commitment: value.newStateCommitment,
            number_of_layer1_txs: value.numberOfLayer1Txs.to::<u64>(),
            number_of_layer2_txs: 0,
            priority_operations_hash: value.priorityOperationsHash,
            dependency_roots_rolling_hash: value.dependencyRootsRollingHash,
            l2_to_l1_logs_root_hash: value.l2LogsTreeRoot,
            l2_da_commitment_scheme: value.daCommitmentScheme.into(),
            da_commitment: value.daCommitment,
            first_block_timestamp: value.firstBlockTimestamp,
            first_block_number: Some(value.firstBlockNumber),
            last_block_timestamp: value.lastBlockTimestamp,
            last_block_number: Some(value.lastBlockNumber),
            chain_id: value.chainId.to::<u64>(),
            operator_da_input: value.operatorDAInput.as_ref().to_vec(),
            sl_chain_id: 0,
        }
    }
}

impl fmt::Debug for CommitBatchInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommitBatchInfo")
            .field("batch_number", &self.batch_number)
            .field("new_state_commitment", &self.new_state_commitment)
            .field("number_of_layer1_txs", &self.number_of_layer1_txs)
            .field("number_of_layer2_txs", &self.number_of_layer2_txs)
            .field("priority_operations_hash", &self.priority_operations_hash)
            .field(
                "dependency_roots_rolling_hash",
                &self.dependency_roots_rolling_hash,
            )
            .field("l2_to_l1_logs_root_hash", &self.l2_to_l1_logs_root_hash)
            .field("l2_da_commitment_scheme", &self.l2_da_commitment_scheme)
            .field("da_commitment", &self.da_commitment)
            .field("first_block_timestamp", &self.first_block_timestamp)
            .field("last_block_timestamp", &self.last_block_timestamp)
            .field("chain_id", &self.chain_id)
            // .field("operator_da_input", skipped to keep concise!)
            .finish()
    }
}
