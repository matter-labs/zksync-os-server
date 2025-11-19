use alloy::consensus::{BlobTransactionSidecar, SidecarBuilder, SimpleCoder};
use alloy::primitives::{Address, B256, U256, keccak256};
use blake2::{Blake2s256, Digest};
use ruint::aliases::B160;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};
use zksync_os_contract_interface::models::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_interface::types::{BlockContext, BlockOutput};
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_types::{
    L2_TO_L1_TREE_SIZE, L2ToL1Log, ProtocolSemanticVersion, PubdataMode, ZkEnvelope, ZkTransaction,
};

const PUBDATA_SOURCE_CALLDATA: u8 = 0;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct BatchInfo {
    #[serde(flatten)]
    pub commit_info: CommitBatchInfo,
    /// Chain's diamond proxy address on L1.
    // todo: this should not be a part of this struct as this is static information for the entire chain
    //       but we cannot remove it without breaking backwards compatibility
    pub chain_address: Address,
    /// L1 protocol upgrade transaction that was finalized in this batch. Missing for the vast
    /// majority of batches.
    pub upgrade_tx_hash: Option<B256>,
    /// Blobs sidecar that should be sent with commit operation.
    pub blob_sidecar: Option<BlobTransactionSidecar>,
}

impl BatchInfo {
    pub fn new(
        blocks: Vec<(
            &BlockOutput,
            &BlockContext,
            &[ZkTransaction],
            &zksync_os_merkle_tree::TreeBatchOutput,
        )>,
        chain_id: u64,
        chain_address: Address,
        batch_number: u64,
        pubdata_mode: PubdataMode,
    ) -> Self {
        let mut priority_operations_hash = keccak256([]);
        let mut number_of_layer1_txs = 0;
        let mut total_pubdata = vec![];
        let mut encoded_l2_l1_logs = vec![];

        let (first_block_output, _, _, _) = *blocks.first().unwrap();
        let (last_block_output, last_block_context, _, last_block_tree) = *blocks.last().unwrap();

        let mut upgrade_tx_hash = None;
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
                    ZkEnvelope::Upgrade(_) => {
                        assert!(
                            upgrade_tx_hash.is_none(),
                            "more than one upgrade tx in a batch: first {upgrade_tx_hash:?}, second {}",
                            tx.hash()
                        );
                        upgrade_tx_hash = Some(*tx.hash());
                    }
                }
            }

            for tx_output in block_output.tx_results.clone().into_iter().flatten() {
                encoded_l2_l1_logs.extend(tx_output.l2_to_l1_logs.into_iter().map(
                    |log_with_preimage| {
                        let log = L2ToL1Log {
                            l2_shard_id: log_with_preimage.log.l2_shard_id,
                            is_service: log_with_preimage.log.is_service,
                            tx_number_in_block: log_with_preimage.log.tx_number_in_block,
                            sender: log_with_preimage.log.sender,
                            key: log_with_preimage.log.key,
                            value: log_with_preimage.log.value,
                        };
                        log.encode()
                    },
                ));
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
        let da_fields = calculate_da_fields(&total_pubdata, pubdata_mode);

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

        let commit_info = CommitBatchInfo {
            batch_number,
            new_state_commitment,
            number_of_layer1_txs,
            priority_operations_hash,
            dependency_roots_rolling_hash: B256::ZERO,
            l2_to_l1_logs_root_hash,
            l2_da_commitment_scheme: pubdata_mode.da_commitment_scheme(),
            da_commitment: da_fields.da_commitment,
            first_block_timestamp: first_block_output.header.timestamp,
            first_block_number: Some(first_block_output.header.number),
            last_block_timestamp: last_block_output.header.timestamp,
            last_block_number: Some(last_block_output.header.number),
            chain_id,
            operator_da_input: da_fields.operator_da_input,
        };
        Self {
            commit_info,
            chain_address,
            upgrade_tx_hash,
            blob_sidecar: da_fields.blob_sidecar,
        }
    }

    /// Calculate keccak256 hash of BatchOutput part of public input
    pub fn public_input_hash(&self, protocol_version: &ProtocolSemanticVersion) -> B256 {
        let commit_info = &self.commit_info;
        match protocol_version.minor {
            29 => {
                use zk_ee_0_1_0::utils::Bytes32;
                let system_batch_output =
                    zk_os_basic_system_0_1_0::system_implementation::system::BatchOutput {
                        chain_id: U256::from(commit_info.chain_id),
                        first_block_timestamp: commit_info.first_block_timestamp,
                        last_block_timestamp: commit_info.last_block_timestamp,
                        // we always set it to zero
                        used_l2_da_validator_address: B160::ZERO,
                        pubdata_commitment: Bytes32::from(commit_info.da_commitment.0),
                        number_of_layer_1_txs: U256::from(commit_info.number_of_layer1_txs),
                        priority_operations_hash: Bytes32::from(
                            commit_info.priority_operations_hash.0,
                        ),
                        l2_logs_tree_root: Bytes32::from(commit_info.l2_to_l1_logs_root_hash.0),
                        upgrade_tx_hash: self
                            .upgrade_tx_hash
                            .map(|h| Bytes32::from_array(h.0))
                            .unwrap_or(Bytes32::ZERO),
                        interop_root_rolling_hash: Bytes32::from(
                            commit_info.dependency_roots_rolling_hash.0,
                        ),
                    };
                B256::from(system_batch_output.hash())
            }
            // 31 needed for upgrade integration test
            30 | 31 => {
                use zk_ee::utils::Bytes32;
                let system_batch_output =
                    zk_os_basic_system::system_implementation::system::BatchOutput {
                        chain_id: U256::from(commit_info.chain_id),
                        first_block_timestamp: commit_info.first_block_timestamp,
                        last_block_timestamp: commit_info.last_block_timestamp,
                        da_commitment_scheme: (commit_info.l2_da_commitment_scheme as u8)
                            .try_into()
                            .expect("Failed to convert DA commitment scheme"),
                        pubdata_commitment: Bytes32::from(commit_info.da_commitment.0),
                        number_of_layer_1_txs: U256::from(commit_info.number_of_layer1_txs),
                        priority_operations_hash: Bytes32::from(
                            commit_info.priority_operations_hash.0,
                        ),
                        l2_logs_tree_root: Bytes32::from(commit_info.l2_to_l1_logs_root_hash.0),
                        upgrade_tx_hash: self
                            .upgrade_tx_hash
                            .map(|h| Bytes32::from_array(h.0))
                            .unwrap_or(Bytes32::ZERO),
                        interop_root_rolling_hash: Bytes32::from(
                            commit_info.dependency_roots_rolling_hash.0,
                        ),
                    };
                B256::from(system_batch_output.hash())
            }
            _ => panic!("Unsupported protocol version: {protocol_version}"),
        }
    }

    pub fn into_stored(self, protocol_version: &ProtocolSemanticVersion) -> StoredBatchInfo {
        let commitment = self.public_input_hash(protocol_version);
        let commit_info = self.commit_info;
        StoredBatchInfo {
            batch_number: commit_info.batch_number,
            state_commitment: commit_info.new_state_commitment,
            number_of_layer1_txs: commit_info.number_of_layer1_txs,
            priority_operations_hash: commit_info.priority_operations_hash,
            dependency_roots_rolling_hash: commit_info.dependency_roots_rolling_hash,
            l2_to_l1_logs_root_hash: commit_info.l2_to_l1_logs_root_hash,
            commitment,
            last_block_timestamp: commit_info.last_block_timestamp,
        }
    }
}

impl Deref for BatchInfo {
    type Target = CommitBatchInfo;

    fn deref(&self) -> &Self::Target {
        &self.commit_info
    }
}

impl DerefMut for BatchInfo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.commit_info
    }
}

struct DAFields {
    pub da_commitment: B256,
    pub operator_da_input: Vec<u8>,
    pub blob_sidecar: Option<BlobTransactionSidecar>,
}

fn calculate_da_fields(pubdata: &[u8], pubdata_mode: PubdataMode) -> DAFields {
    let (da_commitment, operator_da_input, blob_sidecar) = match pubdata_mode {
        PubdataMode::Validium => (B256::ZERO, vec![0u8; 32], None),
        PubdataMode::Calldata => {
            let mut operator_da_input = Vec::with_capacity(32 * 3 + 1 + pubdata.len() + 1 + 32);

            // reference for this header is taken from zk_ee: https://github.com/matter-labs/zk_ee/blob/ad-aggregation-program/aggregator/src/aggregation/da_commitment.rs#L27
            // consider reusing that code instead:
            //
            // hasher.update([0u8; 32]); // we don't have to validate state diffs hash
            // hasher.update(Keccak256::digest(&pubdata)); // full pubdata keccak
            // hasher.update([1u8]); // with calldata we should provide 1 blob
            // hasher.update([0u8; 32]); // its hash will be ignored on the settlement layer
            // Ok(hasher.finalize().into())

            operator_da_input.extend(B256::ZERO.as_slice());
            operator_da_input.extend(keccak256(pubdata));
            operator_da_input.push(1);
            operator_da_input.extend(B256::ZERO.as_slice());

            //     bytes32 daCommitment; - we compute hash of the first part of the operator_da_input (see above)
            let da_commitment = keccak256(&operator_da_input);

            operator_da_input.extend([PUBDATA_SOURCE_CALLDATA]);
            operator_da_input.extend(pubdata);
            // blob_commitment should be set to zero in ZK OS
            operator_da_input.extend(B256::ZERO.as_slice());

            (da_commitment, operator_da_input, None)
        }
        PubdataMode::Blobs => {
            // returns error in case of internal error during sidecar calculation
            let blob_sidecar = SidecarBuilder::<SimpleCoder>::from_slice(pubdata)
                .build()
                .unwrap();
            let versioned_hashes: Vec<u8> = blob_sidecar
                .versioned_hashes()
                .flat_map(|hash| hash.0.to_vec())
                .collect();
            let da_commitment = keccak256(&versioned_hashes);

            // we place zeroes into da input to publish blobs with commit transaction
            let operator_da_input = vec![0u8; versioned_hashes.len()];
            (da_commitment, operator_da_input, Some(blob_sidecar))
        }
    };
    DAFields {
        da_commitment,
        operator_da_input,
        blob_sidecar,
    }
}
