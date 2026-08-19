//! Server-local interface for native batch prover input generation.
//!
//! This crate isolates version-specific zksync-os native batching APIs from the rest of the
//! server so `multivm` can stay focused on block execution and transaction simulation.

use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::{B256, keccak256};
use zksync_os_batch_types::{BlockMerkleTreeData, CanonicalBatchCommitData, PendingBatchInfo};
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::{ProtocolSemanticVersion, ProvingVersion, PubdataMode};

pub mod tree;
mod v32;

/// Per-block input to [`generate_batch_run`].
#[derive(Debug, Clone, Copy)]
pub struct NativeBatchBlock<'a> {
    pub replay_record: &'a ReplayRecord,
    pub tree_data: &'a BlockMerkleTreeData,
}

#[derive(Debug)]
pub struct NativeBatchRunOutput {
    pub prover_input: Vec<u32>,
    pub pubdata: Vec<u8>,
    /// State commitment before the batch, as seen by the batch program (public input
    /// `state_before`).
    pub previous_state_commitment: B256,
    /// keccak256 of the full batch public input computed by the zksync-os batch program:
    /// `keccak(state_before || state_after || chain_config_hash || batch_output)`. This is the
    /// value a FRI proof of this batch exposes in its final registers; server-side proof
    /// verification must reconstruct exactly this hash.
    pub batch_public_input_hash: B256,
    pub new_state_commitment: B256,
    pub da_commitment: B256,
    pub number_of_layer1_txs: u64,
    pub number_of_layer2_txs: u64,
    pub priority_operations_hash: B256,
    pub dependency_roots_rolling_hash: B256,
    pub l2_to_l1_logs_root_hash: B256,
    pub first_block_timestamp: u64,
    pub last_block_timestamp: u64,
    pub chain_id: u64,
    pub sl_chain_id: u64,
    pub upgrade_tx_hash: Option<B256>,
}

impl NativeBatchRunOutput {
    pub fn canonical_commit_data(
        &self,
        first_block_number: u64,
        last_block_number: u64,
    ) -> CanonicalBatchCommitData {
        CanonicalBatchCommitData {
            first_block_number,
            last_block_number,
            first_block_timestamp: self.first_block_timestamp,
            last_block_timestamp: self.last_block_timestamp,
            new_state_commitment: self.new_state_commitment,
            da_commitment: self.da_commitment,
            number_of_layer1_txs: self.number_of_layer1_txs,
            number_of_layer2_txs: self.number_of_layer2_txs,
            priority_operations_hash: self.priority_operations_hash,
            dependency_roots_rolling_hash: self.dependency_roots_rolling_hash,
            l2_to_l1_logs_root_hash: self.l2_to_l1_logs_root_hash,
            upgrade_tx_hash: self.upgrade_tx_hash,
            chain_id: self.chain_id,
            sl_chain_id: self.sl_chain_id,
            pubdata: self.pubdata.clone(),
        }
    }

    /// Builds the batch commit info from this run, cross-checking the run output against the
    /// node-side expectations so that a divergence fails at seal/verification time rather than
    /// as an opaque proof mismatch hours later.
    #[allow(clippy::too_many_arguments)]
    pub fn build_batch_info(
        &self,
        batch_number: u64,
        first_block_number: u64,
        last_block_number: u64,
        pubdata_mode: PubdataMode,
        protocol_version: &ProtocolSemanticVersion,
        chain_id: u64,
        sl_chain_id: u64,
    ) -> anyhow::Result<(PendingBatchInfo, Option<BlobTransactionSidecar>)> {
        anyhow::ensure!(
            self.chain_id == chain_id,
            "native batch run chain id mismatch: node has {chain_id}, batch program used {}",
            self.chain_id,
        );
        anyhow::ensure!(
            self.sl_chain_id == sl_chain_id,
            "native batch run SL chain id mismatch: node has {sl_chain_id}, state has {}",
            self.sl_chain_id,
        );

        let (batch_info, blob_sidecar) = PendingBatchInfo::build_from_canonical_output(
            batch_number,
            pubdata_mode,
            protocol_version,
            self.canonical_commit_data(first_block_number, last_block_number),
        )?;

        // Reconstruct the batch public input exactly as `verify_fri_proof_v8` will and compare
        // it with the value the batch program computed; catches batch-output layout drift.
        let chain_config_hash = v32::chain_config_hash(chain_id)?;
        let reconstructed_public_input_hash = keccak256(
            [
                self.previous_state_commitment.0,
                batch_info.commit_info.new_state_commitment.0,
                chain_config_hash.0,
                batch_info.v32_batch_output_hash().0,
            ]
            .concat(),
        );
        anyhow::ensure!(
            reconstructed_public_input_hash == self.batch_public_input_hash,
            "batch public input hash mismatch: server-side reconstruction {reconstructed_public_input_hash}, batch program {}",
            self.batch_public_input_hash,
        );

        Ok((batch_info, blob_sidecar))
    }
}

/// The chain config all v32 executions (block and native batch) run with. Its hash is part of
/// the v32 batch public input, so every construction site must go through this function.
pub fn v32_chain_config(
    chain_id: u64,
) -> anyhow::Result<zk_ee_0_4_0::system::metadata::chain_config::ChainConfig> {
    v32::chain_config(chain_id)
}

/// keccak256 commitment of [`v32_chain_config`], as committed to in the v32 batch public input.
pub fn v32_chain_config_hash(chain_id: u64) -> anyhow::Result<B256> {
    v32::chain_config_hash(chain_id)
}

pub fn generate_batch_run<ReadState: ReadStateHistory>(
    proving_version: ProvingVersion,
    blocks: &[NativeBatchBlock<'_>],
    read_state: &ReadState,
    merkle_tree: MerkleTree<RocksDBWrapper>,
    pubdata_mode: PubdataMode,
) -> anyhow::Result<NativeBatchRunOutput> {
    match proving_version {
        ProvingVersion::V8 => {
            v32::generate_batch_run(blocks, read_state, merkle_tree, pubdata_mode)
        }
        ProvingVersion::V6 | ProvingVersion::V7 => {
            anyhow::bail!("native batch proving is unsupported for {proving_version:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_commit_data_preserves_native_batch_output() {
        let output = NativeBatchRunOutput {
            prover_input: vec![1, 2, 3],
            pubdata: vec![9, 8, 7],
            previous_state_commitment: B256::repeat_byte(0x77),
            batch_public_input_hash: B256::repeat_byte(0x88),
            new_state_commitment: B256::repeat_byte(0x11),
            da_commitment: B256::repeat_byte(0x22),
            number_of_layer1_txs: 3,
            number_of_layer2_txs: 5,
            priority_operations_hash: B256::repeat_byte(0x33),
            dependency_roots_rolling_hash: B256::repeat_byte(0x44),
            l2_to_l1_logs_root_hash: B256::repeat_byte(0x55),
            first_block_timestamp: 100,
            last_block_timestamp: 200,
            chain_id: 270,
            sl_chain_id: 123,
            upgrade_tx_hash: Some(B256::repeat_byte(0x66)),
        };

        let canonical = output.canonical_commit_data(7, 9);

        assert_eq!(canonical.first_block_number, 7);
        assert_eq!(canonical.last_block_number, 9);
        assert_eq!(canonical.first_block_timestamp, 100);
        assert_eq!(canonical.last_block_timestamp, 200);
        assert_eq!(canonical.new_state_commitment, B256::repeat_byte(0x11));
        assert_eq!(canonical.da_commitment, B256::repeat_byte(0x22));
        assert_eq!(canonical.number_of_layer1_txs, 3);
        assert_eq!(canonical.number_of_layer2_txs, 5);
        assert_eq!(canonical.priority_operations_hash, B256::repeat_byte(0x33));
        assert_eq!(
            canonical.dependency_roots_rolling_hash,
            B256::repeat_byte(0x44)
        );
        assert_eq!(canonical.l2_to_l1_logs_root_hash, B256::repeat_byte(0x55));
        assert_eq!(canonical.upgrade_tx_hash, Some(B256::repeat_byte(0x66)));
        assert_eq!(canonical.chain_id, 270);
        assert_eq!(canonical.sl_chain_id, 123);
        assert_eq!(canonical.pubdata, vec![9, 8, 7]);
    }
}
