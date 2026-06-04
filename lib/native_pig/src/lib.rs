//! Server-local interface for native batch prover input generation.
//!
//! This crate isolates version-specific zksync-os native batching APIs from the rest of the
//! server so `multivm` can stay focused on block execution and transaction simulation.

use alloy::primitives::B256;
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::{ProvingVersion, PubdataMode};

mod v8;

#[derive(Debug)]
pub struct NativeBatchRunOutput {
    pub prover_input: Vec<u32>,
    pub pubdata: Vec<u8>,
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
