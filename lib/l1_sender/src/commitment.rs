use alloy::primitives::{Address, B256, Bytes, FixedBytes, U256, keccak256};
use alloy::sol_types::SolValue;
use blake2::{Blake2s256, Digest};
use ruint::aliases::B160;
use serde::{Deserialize, Serialize};
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::{BlockContext, BlockOutput};
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_types::{L2_TO_L1_TREE_SIZE, ZkEnvelope, ZkTransaction};

const PUBDATA_SOURCE_CALLDATA: u8 = 0;

/// User-friendly version of [`zksync_os_contract_interface::IExecutor::StoredBatchInfo`] containing
/// fields that are relevant for ZKsync OS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredBatchInfo {
    pub batch_number: u64,
    pub state_commitment: B256,
    pub number_of_layer1_txs: u64,
    pub priority_operations_hash: B256,
    pub l2_to_l1_logs_root_hash: B256,
    pub commitment: B256,
    pub last_block_timestamp: u64,
}

impl StoredBatchInfo {
    pub fn hash(&self) -> B256 {
        let abi_encoded = zksync_os_contract_interface::IExecutor::StoredBatchInfo::from(self)
            .abi_encode_params();
        keccak256(abi_encoded.as_slice())
    }
}

impl From<CommitBatchInfo> for StoredBatchInfo {
    fn from(value: CommitBatchInfo) -> Self {
        // TODO: This ALSO really needs a different name
        let system_batch_output = zk_os_basic_system::system_implementation::system::BatchOutput {
            chain_id: U256::from(value.chain_id),
            first_block_timestamp: value.first_block_timestamp,
            last_block_timestamp: value.last_block_timestamp,
            used_l2_da_validator_address: B160::from_be_bytes(value.l2_da_validator.into_array()),
            pubdata_commitment: Bytes32::from(value.da_commitment.0),
            number_of_layer_1_txs: U256::from(value.number_of_layer1_txs),
            priority_operations_hash: Bytes32::from(value.priority_operations_hash.0),
            l2_logs_tree_root: Bytes32::from(value.l2_to_l1_logs_root_hash.0),
            // TODO: Presumably shouldn't always be zero once we have upgrade transactions
            upgrade_tx_hash: Default::default(),
        };
        let commitment = FixedBytes::from(system_batch_output.hash());
        Self {
            batch_number: value.batch_number,
            state_commitment: value.new_state_commitment,
            number_of_layer1_txs: value.number_of_layer1_txs,
            priority_operations_hash: value.priority_operations_hash,
            l2_to_l1_logs_root_hash: value.l2_to_l1_logs_root_hash,
            commitment,
            last_block_timestamp: value.last_block_timestamp,
        }
    }
}

impl From<&StoredBatchInfo> for zksync_os_contract_interface::IExecutor::StoredBatchInfo {
    fn from(value: &StoredBatchInfo) -> Self {
        Self::from((
            // `batchNumber`
            value.batch_number,
            // `batchHash` - for ZKsync OS batches we store full state commitment here
            B256::from(value.state_commitment.0),
            // `indexRepeatedStorageChanges` - Not used in Boojum OS, must be zero
            0u64,
            // `numberOfLayer1Txs`
            U256::from(value.number_of_layer1_txs),
            // `priorityOperationsHash`
            B256::from(value.priority_operations_hash.0),
            // `l2LogsTreeRoot`
            B256::from(value.l2_to_l1_logs_root_hash.0),
            // `timestamp` - Not used in ZKsync OS, must be zero
            U256::from(0),
            // `commitment` - For ZKsync OS batches we store batch output hash here
            B256::from(value.commitment.0),
        ))
    }
}

/// User-friendly version of [`zksync_os_contract_interface::IExecutor::CommitBoojumOSBatchInfo`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitBatchInfo {
    pub batch_number: u64,
    pub new_state_commitment: B256,
    pub number_of_layer1_txs: u64,
    pub priority_operations_hash: B256,
    pub l2_to_l1_logs_root_hash: B256,
    pub l2_da_validator: Address,
    pub da_commitment: B256,
    pub first_block_timestamp: u64,
    pub last_block_timestamp: u64,
    pub chain_id: u64,
    pub operator_da_input: Vec<u8>,
}

impl CommitBatchInfo {
    pub fn new(
        blocks: Vec<(
            &BlockOutput,
            &BlockContext,
            &[ZkTransaction],
            &zksync_os_merkle_tree::TreeBatchOutput,
        )>,
        chain_id: u64,
        batch_number: u64,
    ) -> Self {
        let mut priority_operations_hash = keccak256([]);
        let mut number_of_layer1_txs = 0;
        let mut total_pubdata = vec![];
        let mut encoded_l2_l1_logs = vec![];

        let (first_block_output, _, _, _) = *blocks.first().unwrap();
        let (last_block_output, last_block_context, _, last_block_tree) = *blocks.last().unwrap();

        for (block_output, _, transactions, _) in blocks {
            total_pubdata.extend(block_output.pubdata.clone());

            for tx in transactions {
                match tx.envelope() {
                    ZkEnvelope::L1(l1_tx) => {
                        let onchain_data_hash = l1_tx.hash();
                        priority_operations_hash =
                            keccak256([priority_operations_hash.0, onchain_data_hash.0].concat());
                        number_of_layer1_txs += 1;
                    }
                    ZkEnvelope::L2(_) => {}
                }
            }

            for tx_output in block_output.tx_results.clone().into_iter().flatten() {
                encoded_l2_l1_logs.extend(
                    tx_output
                        .l2_to_l1_logs
                        .into_iter()
                        .map(|log_with_preimage| log_with_preimage.log.encode()),
                );
            }
        }

        let last_256_block_hashes_blake = {
            let mut blocks_hasher = Blake2s256::new();
            for block_hash in &last_block_context.block_hashes.0[1..] {
                blocks_hasher.update(block_hash.to_be_bytes::<32>());
            }
            blocks_hasher.update(last_block_output.header.hash());

            blocks_hasher.finalize()
        };

        /* ---------- operator DA input ---------- */
        let mut operator_da_input: Vec<u8> = vec![];

        // reference for this header is taken from zk_ee: https://github.com/matter-labs/zk_ee/blob/ad-aggregation-program/aggregator/src/aggregation/da_commitment.rs#L27
        // consider reusing that code instead:
        //
        // hasher.update([0u8; 32]); // we don't have to validate state diffs hash
        // hasher.update(Keccak256::digest(&pubdata)); // full pubdata keccak
        // hasher.update([1u8]); // with calldata we should provide 1 blob
        // hasher.update([0u8; 32]); // its hash will be ignored on the settlement layer
        // Ok(hasher.finalize().into())

        operator_da_input.extend(B256::ZERO.as_slice());
        operator_da_input.extend(keccak256(&total_pubdata));
        operator_da_input.push(1);
        operator_da_input.extend(B256::ZERO.as_slice());

        //     bytes32 daCommitment; - we compute hash of the first part of the operator_da_input (see above)
        let operator_da_input_header_hash = keccak256(&operator_da_input);

        operator_da_input.extend([PUBDATA_SOURCE_CALLDATA]);
        operator_da_input.extend(&total_pubdata);
        // blob_commitment should be set to zero in ZK OS
        operator_da_input.extend(B256::ZERO.as_slice());

        /* ---------- new state commitment ---------- */
        let mut hasher = Blake2s256::new();
        hasher.update(last_block_tree.root_hash.as_slice());
        hasher.update(last_block_tree.leaf_count.to_be_bytes());
        hasher.update(last_block_output.header.number.to_be_bytes());
        hasher.update(last_256_block_hashes_blake);
        hasher.update(last_block_output.header.timestamp.to_be_bytes());
        let new_state_commitment = B256::from_slice(&hasher.finalize());

        /* ---------- root hash of l2->l1 logs ---------- */
        let l2_l1_local_root = MiniMerkleTree::new(
            encoded_l2_l1_logs.clone().into_iter(),
            Some(L2_TO_L1_TREE_SIZE),
        )
        .merkle_root();
        // The result should be Keccak(l2_l1_local_root, aggreagation_root) - we don't compute aggregation root yet
        let l2_to_l1_logs_root_hash = keccak256([l2_l1_local_root.0, [0u8; 32]].concat());

        Self {
            batch_number,
            new_state_commitment,
            number_of_layer1_txs,
            priority_operations_hash,
            l2_to_l1_logs_root_hash,
            // TODO: Update once enforced, not sure where to source it from yet
            l2_da_validator: Default::default(),
            da_commitment: operator_da_input_header_hash,
            first_block_timestamp: first_block_output.header.timestamp,
            last_block_timestamp: last_block_output.header.timestamp,
            chain_id,
            operator_da_input,
        }
    }
}

impl From<CommitBatchInfo> for zksync_os_contract_interface::IExecutor::CommitBoojumOSBatchInfo {
    fn from(value: CommitBatchInfo) -> Self {
        Self::from((
            value.batch_number,
            value.new_state_commitment,
            U256::from(value.number_of_layer1_txs),
            value.priority_operations_hash,
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
