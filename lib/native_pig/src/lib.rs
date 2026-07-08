//! Server-local interface for native batch prover input generation.
//!
//! This crate isolates version-specific zksync-os native batching APIs from the rest of the
//! server so `multivm` can stay focused on block execution and transaction simulation.

use alloy::primitives::B256;
use zksync_os_batch_types::CanonicalBatchCommitData;
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::{ProvingVersion, PubdataMode};

mod v8;

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
}

/// keccak256 commitment of the chain config used by V8 native batch runs (and thus committed
/// to in the V8 batch public input). Must stay in sync with the `ChainConfig` constructed in
/// [`v8::generate_batch_run`].
pub fn v8_chain_config_hash(chain_id: u64) -> anyhow::Result<B256> {
    v8::chain_config_hash(chain_id)
}

pub fn generate_batch_run<ReadState: ReadStateHistory>(
    proving_version: ProvingVersion,
    replay_records: &[ReplayRecord],
    read_state: &ReadState,
    merkle_tree: MerkleTree<RocksDBWrapper>,
    pubdata_mode: PubdataMode,
) -> anyhow::Result<NativeBatchRunOutput> {
    match proving_version {
        ProvingVersion::V8 => {
            v8::generate_batch_run(replay_records, read_state, merkle_tree, pubdata_mode)
        }
        _ => anyhow::bail!("native batch proving is unsupported for {proving_version:?}"),
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
