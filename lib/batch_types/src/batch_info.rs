use alloy::consensus::{BlobTransactionSidecar, SidecarBuilder, SimpleCoder};
use alloy::primitives::{Address, B256, BlockNumber, U256, keccak256};
use alloy::sol_types::SolValue;
use blake2::{Blake2s256, Digest};
use serde::{Deserialize, Serialize};
use std::ops;
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
    #[allow(clippy::too_many_arguments)]
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
        sl_chain_id: u64,
        multichain_root: B256,
        protocol_version: &ProtocolSemanticVersion,
    ) -> Self {
        let mut priority_operations_hash = keccak256([]);
        let mut number_of_layer1_txs = 0;
        let mut number_of_layer2_txs = 0;
        let mut total_pubdata = vec![];
        let mut encoded_l2_l1_logs = vec![];

        let (first_block_output, _, _, _) = *blocks.first().unwrap();
        let (last_block_output, last_block_context, _, last_block_tree) = *blocks.last().unwrap();

        let mut upgrade_tx_hash = None;

        let mut dependency_roots_rolling_hash = B256::ZERO;

        for (block_output, _, transactions, _) in blocks {
            total_pubdata.extend(block_output.pubdata.clone());

            for tx in transactions {
                match tx.envelope() {
                    ZkEnvelope::System(envelope) => {
                        number_of_layer2_txs += 1;

                        if let Some(roots) = envelope.interop_roots() {
                            for root in roots {
                                dependency_roots_rolling_hash = keccak256(
                                    (
                                        dependency_roots_rolling_hash,
                                        root.chainId,
                                        root.blockOrBatchNumber,
                                        root.sides,
                                    )
                                        .abi_encode_packed(),
                                );
                            }
                        }
                    }
                    ZkEnvelope::L2(_) => {
                        number_of_layer2_txs += 1;
                    }
                    ZkEnvelope::L1(l1_tx) => {
                        let onchain_data_hash = l1_tx.hash();
                        priority_operations_hash =
                            keccak256([priority_operations_hash.0, onchain_data_hash.0].concat());
                        number_of_layer1_txs += 1;
                    }
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
        let da_fields = calculate_da_fields(
            &total_pubdata,
            pubdata_mode,
            last_block_context.execution_version,
        );

        /* ---------- new state commitment ---------- */
        // FIXME: extract to a type common batch types?
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

        let l2_to_l1_logs_root_hash = if protocol_version.is_post_v31() {
            // The result should be Keccak(l2_l1_local_root, multichain_root).
            keccak256([l2_l1_local_root.0, multichain_root.0].concat())
        } else {
            // For older protocol versions, multichain root should be set to zero.
            keccak256([l2_l1_local_root.0, [0u8; 32]].concat())
        };

        let commit_info = CommitBatchInfo {
            batch_number,
            new_state_commitment,
            number_of_layer1_txs,
            number_of_layer2_txs,
            priority_operations_hash,
            dependency_roots_rolling_hash,
            l2_to_l1_logs_root_hash,
            l2_da_commitment_scheme: pubdata_mode.da_commitment_scheme(),
            da_commitment: da_fields.da_commitment,
            first_block_timestamp: first_block_output.header.timestamp,
            first_block_number: Some(first_block_output.header.number),
            last_block_timestamp: last_block_output.header.timestamp,
            last_block_number: Some(last_block_output.header.number),
            chain_id,
            operator_da_input: da_fields.operator_da_input,
            sl_chain_id,
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
        let upgrade_tx_hash = self.upgrade_tx_hash.unwrap_or(B256::ZERO);
        match protocol_version.minor {
            // v30 and v31 use different packed layouts for batch output hash:
            // v31 inserts number_of_layer2_txs between L1 tx count and priority_operations_hash.
            30 => B256::from(keccak256(
                (
                    U256::from(commit_info.chain_id),
                    commit_info.first_block_timestamp,
                    commit_info.last_block_timestamp,
                    U256::from(commit_info.l2_da_commitment_scheme as u8),
                    commit_info.da_commitment,
                    U256::from(commit_info.number_of_layer1_txs),
                    commit_info.priority_operations_hash,
                    commit_info.l2_to_l1_logs_root_hash,
                    upgrade_tx_hash,
                    commit_info.dependency_roots_rolling_hash,
                )
                    .abi_encode_packed(),
            )),
            31 | 32 => B256::from(keccak256(
                (
                    U256::from(commit_info.chain_id),
                    commit_info.first_block_timestamp,
                    commit_info.last_block_timestamp,
                    U256::from(commit_info.l2_da_commitment_scheme as u8),
                    commit_info.da_commitment,
                    U256::from(commit_info.number_of_layer1_txs),
                    U256::from(commit_info.number_of_layer2_txs),
                    commit_info.priority_operations_hash,
                    commit_info.l2_to_l1_logs_root_hash,
                    upgrade_tx_hash,
                    commit_info.dependency_roots_rolling_hash,
                    U256::from(commit_info.sl_chain_id),
                )
                    .abi_encode_packed(),
            )),
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
            // unused
            last_block_timestamp: Some(0),
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

fn calculate_da_fields(
    pubdata: &[u8],
    pubdata_mode: PubdataMode,
    batch_execution_version: u32,
) -> DAFields {
    let (da_commitment, operator_da_input, blob_sidecar) =
        match (pubdata_mode, batch_execution_version) {
            (PubdataMode::Calldata | PubdataMode::RelayedL2Calldata, _)
            | (PubdataMode::Validium, 4) => {
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

                if pubdata_mode == PubdataMode::Validium {
                    operator_da_input = U256::ZERO.to_be_bytes_vec();
                }

                (da_commitment, operator_da_input, None)
            }
            (PubdataMode::Validium, _) => (B256::ZERO, vec![0u8; 32], None),
            (PubdataMode::Blobs, _) => {
                // returns error in case of internal error during sidecar calculation
                let blob_sidecar: BlobTransactionSidecar =
                    SidecarBuilder::<SimpleCoder>::from_slice(pubdata)
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveredCommittedBatch {
    /// Information about committed batch as was discovered on-chain.
    pub batch_info: StoredBatchInfo,
    /// Range of L2 blocks that belong to this batch.
    pub block_range: ops::RangeInclusive<BlockNumber>,
}

impl DiscoveredCommittedBatch {
    pub fn number(&self) -> u64 {
        self.batch_info.batch_number
    }

    pub fn hash(&self) -> B256 {
        self.batch_info.hash()
    }

    pub fn first_block_number(&self) -> BlockNumber {
        *self.block_range.start()
    }

    pub fn last_block_number(&self) -> BlockNumber {
        *self.block_range.end()
    }

    pub fn block_count(&self) -> u64 {
        self.block_range.end() - self.block_range.start() + 1
    }
}
