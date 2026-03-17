use alloy::primitives::{Bytes, U256};
use std::fmt;
use structdiff::Difference;
use structdiff::StructDiff;
use zksync_os_contract_interface::{
    IExecutorV30,
    models::{CommitBatchInfo, DACommitmentScheme},
};

/// Canonical batch-verification payload used at the transport boundary.
///
/// This struct is the separation layer between the batch-verification transport and the
/// contract ABI. The wire protocol serializes this payload instead of treating the ABI
/// structs themselves as transport objects.
///
/// That separation lets us integrate ABI changes while keeping the wire format pinned to
/// the legacy layout that old ENs already understand. Before the protocol upgrade, any ABI
/// change that affects batch verification should be reflected here first, then rolled out to
/// all ENs, and only then to the main node.
///
/// When the protocol upgrade is ready, update this payload deliberately and bump the wire
/// format together.
#[derive(Clone, PartialEq, Difference)]
#[difference(expose)]
pub struct BatchVerificationCommitInfo {
    pub batch_number: u64,
    pub new_state_commitment: alloy::primitives::B256,
    pub number_of_layer1_txs: u64,
    pub priority_operations_hash: alloy::primitives::B256,
    pub dependency_roots_rolling_hash: alloy::primitives::B256,
    pub l2_to_l1_logs_root_hash: alloy::primitives::B256,
    pub l2_da_commitment_scheme: DACommitmentScheme,
    pub da_commitment: alloy::primitives::B256,
    pub first_block_timestamp: u64,
    pub first_block_number: u64,
    pub last_block_timestamp: u64,
    pub last_block_number: u64,
    pub chain_id: u64,
    pub operator_da_input: Vec<u8>,
}

impl fmt::Debug for BatchVerificationCommitInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchVerificationCommitInfo")
            .field("batch_number", &self.batch_number)
            .field("new_state_commitment", &self.new_state_commitment)
            .field("number_of_layer1_txs", &self.number_of_layer1_txs)
            .field("priority_operations_hash", &self.priority_operations_hash)
            .field(
                "dependency_roots_rolling_hash",
                &self.dependency_roots_rolling_hash,
            )
            .field("l2_to_l1_logs_root_hash", &self.l2_to_l1_logs_root_hash)
            .field("l2_da_commitment_scheme", &self.l2_da_commitment_scheme)
            .field("da_commitment", &self.da_commitment)
            .field("first_block_timestamp", &self.first_block_timestamp)
            .field("first_block_number", &self.first_block_number)
            .field("last_block_timestamp", &self.last_block_timestamp)
            .field("last_block_number", &self.last_block_number)
            .field("chain_id", &self.chain_id)
            // .field("operator_da_input", skipped to keep concise!)
            .finish()
    }
}

impl From<CommitBatchInfo> for BatchVerificationCommitInfo {
    fn from(value: CommitBatchInfo) -> Self {
        Self {
            batch_number: value.batch_number,
            new_state_commitment: value.new_state_commitment,
            number_of_layer1_txs: value.number_of_layer1_txs,
            priority_operations_hash: value.priority_operations_hash,
            dependency_roots_rolling_hash: value.dependency_roots_rolling_hash,
            l2_to_l1_logs_root_hash: value.l2_to_l1_logs_root_hash,
            l2_da_commitment_scheme: value.l2_da_commitment_scheme,
            da_commitment: value.da_commitment,
            first_block_timestamp: value.first_block_timestamp,
            first_block_number: value
                .first_block_number
                .expect("batch verification transport requires CommitBatchInfo.first_block_number"),
            last_block_timestamp: value.last_block_timestamp,
            last_block_number: value
                .last_block_number
                .expect("batch verification transport requires CommitBatchInfo.last_block_number"),
            chain_id: value.chain_id,
            operator_da_input: value.operator_da_input,
        }
    }
}

impl From<IExecutorV30::CommitBatchInfoZKsyncOS> for BatchVerificationCommitInfo {
    fn from(value: IExecutorV30::CommitBatchInfoZKsyncOS) -> Self {
        Self {
            batch_number: value.batchNumber,
            new_state_commitment: value.newStateCommitment,
            number_of_layer1_txs: value.numberOfLayer1Txs.to::<u64>(),
            priority_operations_hash: value.priorityOperationsHash,
            dependency_roots_rolling_hash: value.dependencyRootsRollingHash,
            l2_to_l1_logs_root_hash: value.l2LogsTreeRoot,
            l2_da_commitment_scheme: value.daCommitmentScheme.into(),
            da_commitment: value.daCommitment,
            first_block_timestamp: value.firstBlockTimestamp,
            first_block_number: value.firstBlockNumber,
            last_block_timestamp: value.lastBlockTimestamp,
            last_block_number: value.lastBlockNumber,
            chain_id: value.chainId.to::<u64>(),
            operator_da_input: value.operatorDAInput.as_ref().to_vec(),
        }
    }
}

impl From<BatchVerificationCommitInfo> for IExecutorV30::CommitBatchInfoZKsyncOS {
    fn from(value: BatchVerificationCommitInfo) -> Self {
        Self::from((
            value.batch_number,
            value.new_state_commitment,
            U256::from(value.number_of_layer1_txs),
            value.priority_operations_hash,
            value.dependency_roots_rolling_hash,
            value.l2_to_l1_logs_root_hash,
            value.l2_da_commitment_scheme.into(),
            value.da_commitment,
            value.first_block_timestamp,
            value.first_block_number,
            value.last_block_timestamp,
            value.last_block_number,
            U256::from(value.chain_id),
            Bytes::from(value.operator_da_input),
        ))
    }
}
