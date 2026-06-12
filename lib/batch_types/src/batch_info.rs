use alloy::consensus::{BlobTransactionSidecar, SidecarBuilder, SimpleCoder};
use alloy::primitives::{B256, BlockNumber, U256, keccak256};
use alloy::sol_types::SolValue;
use blake2::{Blake2s256, Digest};
use serde::{Deserialize, Serialize};
use std::ops;
use std::ops::{Deref, DerefMut};
use zksync_os_contract_interface::models::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_merkle_tree_api::TreeBatchOutput;
use zksync_os_mini_merkle_tree::MiniMerkleTree;
use zksync_os_types::{
    BlockOutput, InteropRoot, L2_TO_L1_LOG_SERIALIZE_SIZE, L2_TO_L1_TREE_SIZE, L2ToL1Log,
    ProtocolSemanticVersion, PubdataMode, ZkEnvelope, ZkTransaction,
};

const PUBDATA_SOURCE_CALLDATA: u8 = 0;

/// Compact per-block inputs for batch commitment reconstruction.
#[derive(Clone, Debug)]
pub struct BlockCommitmentData {
    pub block_number: u64,
    pub timestamp: u64,
    /// L1 priority transaction hashes in execution order.
    pub l1_tx_onchain_hashes: Vec<B256>,
    pub num_l2_txs: u64,
    pub interop_roots: Vec<InteropRoot>,
    pub upgrade_tx_hash: Option<B256>,
    /// Encoded L2->L1 logs in emission order.
    pub encoded_l2_l1_logs: Vec<[u8; L2_TO_L1_LOG_SERIALIZE_SIZE]>,
    pub pubdata: Vec<u8>,
    /// Blake2s over the previous 255 block hashes plus this block hash.
    pub last_256_block_hashes_blake: B256,
    pub tree_root_hash: B256,
    pub tree_leaf_count: u64,
    pub multichain_root: B256,
    pub protocol_version: ProtocolSemanticVersion,
}

impl BlockCommitmentData {
    pub fn new(
        block_output: &BlockOutput,
        transactions: &[ZkTransaction],
        tree_output: &TreeBatchOutput,
        last_256_block_hashes: &[U256; 256],
        multichain_root: B256,
        protocol_version: ProtocolSemanticVersion,
    ) -> Self {
        let mut l1_tx_onchain_hashes = Vec::new();
        let mut num_l2_txs = 0;
        let mut interop_roots = Vec::new();
        let mut upgrade_tx_hash = None;
        for tx in transactions {
            match tx.envelope() {
                ZkEnvelope::System(envelope) => {
                    num_l2_txs += 1;
                    if let Some(roots) = envelope.interop_roots() {
                        interop_roots.extend(roots);
                    }
                }
                ZkEnvelope::L2(_) => num_l2_txs += 1,
                ZkEnvelope::L1(l1_tx) => l1_tx_onchain_hashes.push(*l1_tx.hash()),
                ZkEnvelope::Upgrade(_) => {
                    assert!(
                        upgrade_tx_hash.is_none(),
                        "more than one upgrade tx in a block: first {upgrade_tx_hash:?}, second {}",
                        tx.hash()
                    );
                    upgrade_tx_hash = Some(*tx.hash());
                }
            }
        }

        let encoded_l2_l1_logs = block_output
            .tx_results
            .iter()
            .flatten()
            .flat_map(|tx_output| &tx_output.l2_to_l1_logs)
            .map(|log_with_preimage| {
                L2ToL1Log {
                    l2_shard_id: log_with_preimage.log.l2_shard_id,
                    is_service: log_with_preimage.log.is_service,
                    tx_number_in_block: log_with_preimage.log.tx_number_in_block,
                    sender: log_with_preimage.log.sender,
                    key: log_with_preimage.log.key,
                    value: log_with_preimage.log.value,
                }
                .encode()
            })
            .collect();

        let last_256_block_hashes_blake = {
            let mut blocks_hasher = Blake2s256::new();
            for block_hash in &last_256_block_hashes[1..] {
                blocks_hasher.update(block_hash.to_be_bytes::<32>());
            }
            blocks_hasher.update(block_output.header.hash());
            B256::from_slice(&blocks_hasher.finalize())
        };

        Self {
            block_number: block_output.header.number,
            timestamp: block_output.header.timestamp,
            l1_tx_onchain_hashes,
            num_l2_txs,
            interop_roots,
            upgrade_tx_hash,
            encoded_l2_l1_logs,
            pubdata: block_output.pubdata.clone(),
            last_256_block_hashes_blake,
            tree_root_hash: tree_output.root_hash,
            tree_leaf_count: tree_output.leaf_count,
            multichain_root,
            protocol_version,
        }
    }
}

/// Commitment information about a batch.
/// Contains enough data to restore `StoredBatchInfo` that got applied on-chain.
/// Contains enough data to construct public input hash.
/// todo: these fields should be a part of `CommitBatchInfo` but needs to be changed on L1 contracts' side first
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ExtendedCommitBatchInfo {
    #[serde(flatten)]
    pub commit_info: CommitBatchInfo,
    /// L1 protocol upgrade transaction that was finalized in this batch. Missing for the vast
    /// majority of batches.
    pub upgrade_tx_hash: Option<B256>,
    pub protocol_version: ProtocolSemanticVersion,
}

impl ExtendedCommitBatchInfo {
    pub fn build(
        blocks: &[BlockCommitmentData],
        chain_id: u64,
        batch_number: u64,
        pubdata_mode: PubdataMode,
        sl_chain_id: u64,
    ) -> (Self, Option<BlobTransactionSidecar>) {
        let first_block = blocks.first().expect("batch cannot be empty");
        let last_block = blocks.last().expect("batch cannot be empty");
        // Batch sealing keeps these uniform within a batch.
        let protocol_version = &last_block.protocol_version;

        let mut priority_operations_hash = keccak256([]);
        let mut number_of_layer1_txs = 0;
        let mut number_of_layer2_txs = 0;
        let mut total_pubdata = vec![];
        let mut encoded_l2_l1_logs = vec![];
        let mut upgrade_tx_hash = None;
        let mut dependency_roots_rolling_hash = B256::ZERO;

        for block in blocks {
            total_pubdata.extend_from_slice(&block.pubdata);
            number_of_layer1_txs += block.l1_tx_onchain_hashes.len() as u64;
            number_of_layer2_txs += block.num_l2_txs;
            encoded_l2_l1_logs.extend(block.encoded_l2_l1_logs.iter().copied());

            for onchain_data_hash in &block.l1_tx_onchain_hashes {
                priority_operations_hash =
                    keccak256([priority_operations_hash.0, onchain_data_hash.0].concat());
            }
            for root in &block.interop_roots {
                dependency_roots_rolling_hash = keccak256(
                    (
                        dependency_roots_rolling_hash,
                        root.chainId,
                        root.blockOrBatchNumber,
                        root.sides.clone(),
                    )
                        .abi_encode_packed(),
                );
            }
            if let Some(hash) = block.upgrade_tx_hash {
                assert!(
                    upgrade_tx_hash.is_none(),
                    "more than one upgrade tx in a batch: first {upgrade_tx_hash:?}, second {hash}"
                );
                upgrade_tx_hash = Some(hash);
            }
        }

        /* ---------- operator DA input ---------- */
        let da_fields = calculate_da_fields(&total_pubdata, pubdata_mode);

        /* ---------- new state commitment ---------- */
        // FIXME: extract to a type common batch types?
        let mut hasher = Blake2s256::new();
        hasher.update(last_block.tree_root_hash.as_slice());
        hasher.update(last_block.tree_leaf_count.to_be_bytes());
        hasher.update(last_block.block_number.to_be_bytes());
        hasher.update(last_block.last_256_block_hashes_blake);
        hasher.update(last_block.timestamp.to_be_bytes());
        let new_state_commitment = B256::from_slice(&hasher.finalize());

        /* ---------- root hash of l2->l1 logs ---------- */
        let l2_l1_local_root = MiniMerkleTree::new(
            encoded_l2_l1_logs.clone().into_iter(),
            Some(L2_TO_L1_TREE_SIZE),
        )
        .merkle_root();

        let l2_to_l1_logs_root_hash = if protocol_version.is_post_v31() {
            // The result should be Keccak(l2_l1_local_root, multichain_root).
            keccak256([l2_l1_local_root.0, last_block.multichain_root.0].concat())
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
            first_block_timestamp: first_block.timestamp,
            first_block_number: Some(first_block.block_number),
            last_block_timestamp: last_block.timestamp,
            last_block_number: Some(last_block.block_number),
            chain_id,
            operator_da_input: da_fields.operator_da_input,
            sl_chain_id,
        };
        (
            Self {
                commit_info,
                protocol_version: protocol_version.clone(),
                upgrade_tx_hash,
            },
            da_fields.blob_sidecar,
        )
    }

    /// Calculate keccak256 hash of BatchOutput part of public input
    pub fn public_input_hash(&self) -> B256 {
        let commit_info = &self.commit_info;
        let upgrade_tx_hash = self.upgrade_tx_hash.unwrap_or(B256::ZERO);
        match self.protocol_version.minor {
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
            _ => panic!("Unsupported protocol version: {}", self.protocol_version),
        }
    }

    pub fn into_stored(self) -> StoredBatchInfo {
        let commitment = self.public_input_hash();
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

impl Deref for ExtendedCommitBatchInfo {
    type Target = CommitBatchInfo;

    fn deref(&self) -> &Self::Target {
        &self.commit_info
    }
}

impl DerefMut for ExtendedCommitBatchInfo {
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
        PubdataMode::Calldata | PubdataMode::RelayedL2Calldata => {
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
        PubdataMode::Validium => (B256::ZERO, vec![0u8; 32], None),
        PubdataMode::Blobs => {
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
