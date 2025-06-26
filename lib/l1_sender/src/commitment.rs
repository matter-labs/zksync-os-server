use alloy::sol_types::SolValue;
use blake2::{Blake2s256, Digest};
use serde::{Deserialize, Serialize};
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::BatchOutput;
use zksync_mini_merkle_tree::MiniMerkleTree;
use zksync_types::hasher::keccak::KeccakHasher;
use zksync_types::hasher::Hasher;
use zksync_types::web3::keccak256;
use zksync_types::{Address, ExecuteTransactionCommon, Transaction, H256, U256};

const PUBDATA_SOURCE_CALLDATA: u8 = 0;

/// User-friendly version of [`zksync_os_contract_interface::IExecutor::StoredBatchInfo`] containing
/// fields that are relevant for ZKsync OS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredBatchInfo {
    pub batch_number: u64,
    pub state_commitment: H256,
    pub number_of_layer1_txs: U256,
    pub priority_operations_hash: H256,
    pub l2_to_l1_logs_root_hash: H256,
    pub commitment: H256,
}

impl StoredBatchInfo {
    pub fn hash(&self) -> H256 {
        let abi_encoded = zksync_os_contract_interface::IExecutor::StoredBatchInfo::from(self)
            .abi_encode_params();
        H256(keccak256(abi_encoded.as_slice()))
    }
}

impl From<CommitBatchInfo> for StoredBatchInfo {
    fn from(value: CommitBatchInfo) -> Self {
        // TODO: This ALSO really needs a different name
        let system_batch_output = zk_os_basic_system::system_implementation::system::BatchOutput {
            chain_id: alloy::primitives::U256::from_limbs(value.chain_id.0),
            first_block_timestamp: value.first_block_timestamp,
            last_block_timestamp: value.last_block_timestamp,
            used_l2_da_validator_address: ruint::aliases::B160::from_be_bytes(
                value.l2_da_validator.0,
            ),
            pubdata_commitment: Bytes32::from(value.da_commitment.0),
            number_of_layer_1_txs: ruint::aliases::U256::from_limbs(value.number_of_layer1_txs.0),
            priority_operations_hash: Bytes32::from(value.priority_operations_hash.0),
            l2_logs_tree_root: Bytes32::from(value.l2_to_l1_logs_root_hash.0),
            // TODO: Presumably shouldn't always be zero once we have upgrade transactions
            upgrade_tx_hash: Default::default(),
        };
        let commitment = H256::from(system_batch_output.hash());
        Self {
            batch_number: value.batch_number,
            state_commitment: value.new_state_commitment,
            number_of_layer1_txs: value.number_of_layer1_txs,
            priority_operations_hash: value.priority_operations_hash,
            l2_to_l1_logs_root_hash: value.l2_to_l1_logs_root_hash,
            commitment,
        }
    }
}

impl From<&StoredBatchInfo> for zksync_os_contract_interface::IExecutor::StoredBatchInfo {
    fn from(value: &StoredBatchInfo) -> Self {
        Self::from((
            // `batchNumber`
            value.batch_number,
            // `batchHash` - for ZKsync OS batches we store full state commitment here
            alloy::primitives::FixedBytes::<32>::from(value.state_commitment.0),
            // `indexRepeatedStorageChanges` - Not used in Boojum OS, must be zero
            0u64,
            // `numberOfLayer1Txs`
            alloy::primitives::U256::from_limbs(value.number_of_layer1_txs.0),
            // `priorityOperationsHash`
            alloy::primitives::FixedBytes::<32>::from(value.priority_operations_hash.0),
            // `l2LogsTreeRoot`
            alloy::primitives::FixedBytes::<32>::from(value.l2_to_l1_logs_root_hash.0),
            // `timestamp` - Not used in ZKsync OS, must be zero
            alloy::primitives::U256::from(0),
            // `commitment` - For ZKsync OS batches we store batch output hash here
            alloy::primitives::FixedBytes::<32>::from(value.commitment.0),
        ))
    }
}

/// User-friendly version of [`zksync_os_contract_interface::IExecutor::CommitBoojumOSBatchInfo`].
#[derive(Debug, Clone)]
pub struct CommitBatchInfo {
    pub batch_number: u64,
    pub new_state_commitment: H256,
    pub number_of_layer1_txs: U256,
    pub priority_operations_hash: H256,
    pub l2_to_l1_logs_root_hash: H256,
    pub l2_da_validator: Address,
    pub da_commitment: H256,
    pub first_block_timestamp: u64,
    pub last_block_timestamp: u64,
    pub chain_id: U256,
    pub operator_da_input: Vec<u8>,
}

impl CommitBatchInfo {
    pub fn new(
        batch_output: BatchOutput,
        transactions: Vec<Transaction>,
        // TODO: This really needs a different name
        tree_output: zksync_os_merkle_tree::BatchOutput,
        chain_id: u64,
    ) -> Self {
        let mut l1_tx_count = 0;
        let mut priority_operations_hash: H256 = keccak256(&[]).into();
        for tx in &transactions {
            if let ExecuteTransactionCommon::L1(data) = &tx.common_data {
                l1_tx_count += 1;

                let onchain_metadata = data.onchain_metadata().onchain_data;
                let mut preimage = Vec::new();
                preimage.extend(priority_operations_hash.as_bytes());
                preimage.extend(onchain_metadata.onchain_data_hash.as_bytes());

                priority_operations_hash = keccak256(&preimage).into();
            }
        }

        let mut hasher = Blake2s256::new();
        hasher.update(tree_output.root_hash.as_bytes());
        hasher.update(tree_output.leaf_count.to_be_bytes());
        let new_state_commitment = H256::from_slice(&hasher.finalize());

        let mut operator_da_input: Vec<u8> = vec![];

        // reference for this header is taken from zk_ee: https://github.com/matter-labs/zk_ee/blob/ad-aggregation-program/aggregator/src/aggregation/da_commitment.rs#L27
        // consider reusing that code instead:
        //
        // hasher.update([0u8; 32]); // we don't have to validate state diffs hash
        // hasher.update(Keccak256::digest(&pubdata)); // full pubdata keccak
        // hasher.update([1u8]); // with calldata we should provide 1 blob
        // hasher.update([0u8; 32]); // its hash will be ignored on the settlement layer
        // Ok(hasher.finalize().into())
        operator_da_input.extend(H256::zero().as_bytes());
        operator_da_input.extend(keccak256(&batch_output.pubdata));
        operator_da_input.push(1);
        operator_da_input.extend(H256::zero().as_bytes());

        //     bytes32 daCommitment; - we compute hash of the first part of the operator_da_input (see above)
        let operator_da_input_header_hash: H256 = keccak256(&operator_da_input).into();

        operator_da_input.extend([PUBDATA_SOURCE_CALLDATA]);
        operator_da_input.extend(&batch_output.pubdata);
        // blob_commitment should be set to zero in ZK OS
        operator_da_input.extend(H256::zero().as_bytes());

        let mut encoded_l2_l1_logs = Vec::new();
        for tx_output in batch_output.tx_results.into_iter().flatten() {
            encoded_l2_l1_logs.extend(
                tx_output
                    .l2_to_l1_logs
                    .into_iter()
                    .map(|log_with_preimage| log_with_preimage.log.encode()),
            );
        }
        // todo - extract constant
        let l2_l1_local_root =
            MiniMerkleTree::new(encoded_l2_l1_logs.clone().into_iter(), Some(1 << 14))
                .merkle_root();
        // The result should be Keccak(l2_l1_local_root, aggreagation_root) - we don't compute aggregation root yet
        let l2_to_l1_logs_root_hash = KeccakHasher.compress(&l2_l1_local_root, &H256::zero());

        Self {
            batch_number: batch_output.header.number,
            new_state_commitment,
            number_of_layer1_txs: U256::from(l1_tx_count),
            priority_operations_hash,
            l2_to_l1_logs_root_hash,
            // TODO: Update once enforced, not sure where to source it from yet
            l2_da_validator: Default::default(),
            da_commitment: operator_da_input_header_hash,
            first_block_timestamp: batch_output.header.timestamp,
            last_block_timestamp: batch_output.header.timestamp,
            chain_id: U256::from(chain_id),
            operator_da_input,
        }
    }
}

impl From<CommitBatchInfo> for zksync_os_contract_interface::IExecutor::CommitBoojumOSBatchInfo {
    fn from(value: CommitBatchInfo) -> Self {
        Self::from((
            value.batch_number,
            alloy::primitives::FixedBytes::<32>::from(value.new_state_commitment.0),
            alloy::primitives::U256::from_limbs(value.number_of_layer1_txs.0),
            alloy::primitives::FixedBytes::<32>::from(value.priority_operations_hash.0),
            alloy::primitives::FixedBytes::<32>::from(value.l2_to_l1_logs_root_hash.0),
            alloy::primitives::Address::from(value.l2_da_validator.0),
            alloy::primitives::FixedBytes::<32>::from(value.da_commitment.0),
            value.first_block_timestamp,
            value.last_block_timestamp,
            alloy::primitives::U256::from_limbs(value.chain_id.0),
            alloy::primitives::Bytes::from(value.operator_da_input),
        ))
    }
}
