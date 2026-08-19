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
    BlockOutput, L2_TO_L1_TREE_SIZE, L2ToL1Log, ProtocolSemanticVersion, PubdataMode, ZkEnvelope,
    ZkTransaction,
};

const PUBDATA_SOURCE_CALLDATA: u8 = 0;

/// Information about a batch produced by the batcher and driven through the pipeline before it is
/// committed on-chain.
/// Contains enough data to restore `StoredBatchInfo` that got applied on-chain.
/// Contains enough data to construct public input hash (the batch commitment).
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PendingBatchInfo {
    #[serde(flatten)]
    pub commit_info: CommitBatchInfo,
    /// L1 protocol upgrade transaction that was finalized in this batch. Missing for the vast
    /// majority of batches.
    pub upgrade_tx_hash: Option<B256>,
    pub protocol_version: ProtocolSemanticVersion,
}

/// Batch-level commit values produced canonically by the native batch run: from protocol v32.0
/// the batch program itself computes pubdata, DA/state commitments and L1/L2 tx counters, so
/// [`PendingBatchInfo::build_from_canonical_output`] consumes this instead of the server
/// re-accumulating per-block outputs ([`PendingBatchInfo::build`]).
#[derive(Debug, Clone)]
pub struct CanonicalBatchCommitData {
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub first_block_timestamp: u64,
    pub last_block_timestamp: u64,
    pub new_state_commitment: B256,
    pub da_commitment: B256,
    pub number_of_layer1_txs: u64,
    pub number_of_layer2_txs: u64,
    pub priority_operations_hash: B256,
    pub dependency_roots_rolling_hash: B256,
    pub l2_to_l1_logs_root_hash: B256,
    pub upgrade_tx_hash: Option<B256>,
    pub chain_id: u64,
    pub sl_chain_id: u64,
    pub pubdata: Vec<u8>,
}

impl PendingBatchInfo {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        blocks: Vec<(&BlockOutput, &[ZkTransaction], &TreeBatchOutput)>,
        chain_id: u64,
        batch_number: u64,
        pubdata_mode: PubdataMode,
        sl_chain_id: u64,
        multichain_root: B256,
        protocol_version: &ProtocolSemanticVersion,
        last_256_block_hashes: &[U256; 256],
    ) -> (Self, Option<BlobTransactionSidecar>) {
        let mut priority_operations_hash = keccak256([]);
        let mut number_of_layer1_txs = 0;
        let mut number_of_layer2_txs = 0;
        let mut total_pubdata = vec![];
        let mut encoded_l2_l1_logs = vec![];

        let (first_block_output, _, _) = *blocks.first().unwrap();
        let (last_block_output, _, last_block_tree) = *blocks.last().unwrap();

        let mut upgrade_tx_hash = None;

        let mut dependency_roots_rolling_hash = B256::ZERO;

        for (block_output, transactions, _) in blocks {
            total_pubdata.extend_from_slice(block_output.expect_pubdata_bytes());

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
            for block_hash in &last_256_block_hashes[1..] {
                blocks_hasher.update(block_hash.to_be_bytes::<32>());
            }
            blocks_hasher.update(last_block_output.header.hash());

            blocks_hasher.finalize()
        };

        /* ---------- operator DA input ---------- */
        let da_fields = calculate_da_fields(&total_pubdata, pubdata_mode);

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
        (
            Self {
                commit_info,
                protocol_version: protocol_version.clone(),
                upgrade_tx_hash,
            },
            da_fields.blob_sidecar,
        )
    }

    pub fn build_from_canonical_output(
        batch_number: u64,
        pubdata_mode: PubdataMode,
        protocol_version: &ProtocolSemanticVersion,
        batch: CanonicalBatchCommitData,
    ) -> anyhow::Result<(Self, Option<BlobTransactionSidecar>)> {
        let da_fields = calculate_da_fields(&batch.pubdata, pubdata_mode);
        anyhow::ensure!(
            da_fields.da_commitment == batch.da_commitment,
            "canonical batch DA commitment mismatch: expected {}, got {}",
            batch.da_commitment,
            da_fields.da_commitment,
        );

        let commit_info = CommitBatchInfo {
            batch_number,
            new_state_commitment: batch.new_state_commitment,
            number_of_layer1_txs: batch.number_of_layer1_txs,
            number_of_layer2_txs: batch.number_of_layer2_txs,
            priority_operations_hash: batch.priority_operations_hash,
            dependency_roots_rolling_hash: batch.dependency_roots_rolling_hash,
            l2_to_l1_logs_root_hash: batch.l2_to_l1_logs_root_hash,
            l2_da_commitment_scheme: pubdata_mode.da_commitment_scheme(),
            da_commitment: batch.da_commitment,
            first_block_timestamp: batch.first_block_timestamp,
            first_block_number: Some(batch.first_block_number),
            last_block_timestamp: batch.last_block_timestamp,
            last_block_number: Some(batch.last_block_number),
            chain_id: batch.chain_id,
            operator_da_input: da_fields.operator_da_input,
            sl_chain_id: batch.sl_chain_id,
        };

        Ok((
            Self {
                commit_info,
                upgrade_tx_hash: batch.upgrade_tx_hash,
                protocol_version: protocol_version.clone(),
            },
            da_fields.blob_sidecar,
        ))
    }

    /// Calculate keccak256 hash of BatchOutput part of public input (the batch commitment).
    fn public_input_hash(&self) -> B256 {
        let commit_info = &self.commit_info;
        let upgrade_tx_hash = self.upgrade_tx_hash.unwrap_or(B256::ZERO);
        match self.protocol_version.minor {
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
            31 => B256::from(keccak256(
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
            // v32 drops the leading chain_id - it is committed through the chain config hash
            // in the outer public input instead (era-contracts#2323 does the same on-chain).
            32 => self.v32_batch_output_hash(),
            _ => panic!("Unsupported protocol version: {}", self.protocol_version),
        }
    }

    /// Batch output hash exactly as the zksync-os 0.4.0 (proving V8) batch program computes it
    /// (`BatchOutput::hash` in `basic_bootloader/.../post_tx_op/public_input.rs`): unlike the
    /// pre-V8 [`Self::public_input_hash`] layout, it does NOT include the leading `chain_id` —
    /// the chain id is committed through the chain config hash in the outer batch public input
    /// instead. Used for server-side verification of V8 FRI proofs and as the v32 arm of
    /// [`Self::public_input_hash`] — era-contracts#2323 defines the same layout on-chain.
    pub fn v32_batch_output_hash(&self) -> B256 {
        let commit_info = &self.commit_info;
        let upgrade_tx_hash = self.upgrade_tx_hash.unwrap_or(B256::ZERO);
        B256::from(keccak256(
            (
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
        ))
    }

    /// Computes the batch commitment and turns this into its committed form.
    pub fn into_committed(self) -> CommittedBatchInfo {
        let commitment = self.public_input_hash();
        CommittedBatchInfo {
            commit_info: self.commit_info,
            commitment,
        }
    }

    pub fn into_stored(self) -> StoredBatchInfo {
        self.into_committed().into_stored()
    }
}

impl Deref for PendingBatchInfo {
    type Target = CommitBatchInfo;

    fn deref(&self) -> &Self::Target {
        &self.commit_info
    }
}

impl DerefMut for PendingBatchInfo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.commit_info
    }
}

/// Information about a batch that has already been committed on-chain, as discovered from L1.
/// Carries the batch `commitment` directly (e.g. read from the `BlockCommit` event) instead of
/// the data required to recompute it.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct CommittedBatchInfo {
    #[serde(flatten)]
    pub commit_info: CommitBatchInfo,
    pub commitment: B256,
}

impl CommittedBatchInfo {
    pub fn into_stored(self) -> StoredBatchInfo {
        let commit_info = self.commit_info;
        StoredBatchInfo {
            batch_number: commit_info.batch_number,
            state_commitment: commit_info.new_state_commitment,
            number_of_layer1_txs: commit_info.number_of_layer1_txs,
            priority_operations_hash: commit_info.priority_operations_hash,
            dependency_roots_rolling_hash: commit_info.dependency_roots_rolling_hash,
            l2_to_l1_logs_root_hash: commit_info.l2_to_l1_logs_root_hash,
            commitment: self.commitment,
            // unused
            last_block_timestamp: Some(0),
        }
    }
}

struct DAFields {
    pub da_commitment: B256,
    pub operator_da_input: Vec<u8>,
    pub blob_sidecar: Option<BlobTransactionSidecar>,
}

fn calculate_da_fields(pubdata: &[u8], pubdata_mode: PubdataMode) -> DAFields {
    let (da_commitment, operator_da_input, blob_sidecar) = match pubdata_mode {
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

#[cfg(test)]
mod tests {
    use super::{CanonicalBatchCommitData, PendingBatchInfo, calculate_da_fields};
    use alloy::primitives::B256;
    use zksync_os_types::{ProtocolSemanticVersion, PubdataMode};

    fn canonical_batch_data(pubdata_mode: PubdataMode) -> CanonicalBatchCommitData {
        let pubdata = vec![1, 2, 3, 4, 5, 6];
        let da_fields = calculate_da_fields(&pubdata, pubdata_mode);
        CanonicalBatchCommitData {
            first_block_number: 11,
            last_block_number: 13,
            first_block_timestamp: 100,
            last_block_timestamp: 120,
            new_state_commitment: B256::repeat_byte(0x11),
            da_commitment: da_fields.da_commitment,
            number_of_layer1_txs: 3,
            number_of_layer2_txs: 8,
            priority_operations_hash: B256::repeat_byte(0x22),
            dependency_roots_rolling_hash: B256::repeat_byte(0x33),
            l2_to_l1_logs_root_hash: B256::repeat_byte(0x44),
            upgrade_tx_hash: Some(B256::repeat_byte(0x55)),
            chain_id: 270,
            sl_chain_id: 123,
            pubdata,
        }
    }

    #[test]
    fn builds_commit_info_from_canonical_batch_output() {
        let protocol_version = ProtocolSemanticVersion::new(0, 32, 0);
        let batch = canonical_batch_data(PubdataMode::Calldata);
        let expected_da_fields = calculate_da_fields(&batch.pubdata, PubdataMode::Calldata);

        let (batch_info, blob_sidecar) = PendingBatchInfo::build_from_canonical_output(
            42,
            PubdataMode::Calldata,
            &protocol_version,
            batch,
        )
        .unwrap();

        assert_eq!(batch_info.batch_number, 42);
        assert_eq!(batch_info.new_state_commitment, B256::repeat_byte(0x11));
        assert_eq!(batch_info.number_of_layer1_txs, 3);
        assert_eq!(batch_info.number_of_layer2_txs, 8);
        assert_eq!(batch_info.priority_operations_hash, B256::repeat_byte(0x22));
        assert_eq!(
            batch_info.dependency_roots_rolling_hash,
            B256::repeat_byte(0x33)
        );
        assert_eq!(batch_info.l2_to_l1_logs_root_hash, B256::repeat_byte(0x44));
        assert_eq!(batch_info.upgrade_tx_hash, Some(B256::repeat_byte(0x55)));
        assert_eq!(batch_info.first_block_number, Some(11));
        assert_eq!(batch_info.last_block_number, Some(13));
        assert_eq!(batch_info.first_block_timestamp, 100);
        assert_eq!(batch_info.last_block_timestamp, 120);
        assert_eq!(batch_info.chain_id, 270);
        assert_eq!(batch_info.sl_chain_id, 123);
        assert_eq!(batch_info.da_commitment, expected_da_fields.da_commitment);
        assert_eq!(
            batch_info.operator_da_input,
            expected_da_fields.operator_da_input
        );
        assert!(blob_sidecar.is_none());
    }

    #[test]
    fn detects_canonical_da_commitment_mismatch() {
        let protocol_version = ProtocolSemanticVersion::new(0, 32, 0);
        let mut batch = canonical_batch_data(PubdataMode::Blobs);
        batch.da_commitment = B256::ZERO;

        let err = PendingBatchInfo::build_from_canonical_output(
            42,
            PubdataMode::Blobs,
            &protocol_version,
            batch,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("canonical batch DA commitment mismatch")
        );
    }
}
