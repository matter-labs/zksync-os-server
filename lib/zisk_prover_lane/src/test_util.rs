//! Shared fixtures for the crate's unit tests.

use alloy::primitives::{Address, B256};
use zksync_os_batch_types::PendingBatchInfo;
use zksync_os_batch_types::batcher_model::{
    BatchForSigning, BatchMetadata, BatchSignatureData, SignedBatchEnvelope,
};
use zksync_os_contract_interface::models::{CommitBatchInfo, DACommitmentScheme, StoredBatchInfo};
use zksync_os_types::{ProtocolSemanticVersion, PubdataMode};

/// A minimal all-zero batch envelope carrying `data` as its proof payload.
pub(crate) fn create_test_batch_envelope<E>(batch_number: u64, data: E) -> SignedBatchEnvelope<E> {
    let batch = BatchMetadata {
        previous_stored_batch_info: StoredBatchInfo {
            batch_number: batch_number.saturating_sub(1),
            state_commitment: B256::ZERO,
            number_of_layer1_txs: 0,
            priority_operations_hash: B256::ZERO,
            dependency_roots_rolling_hash: B256::ZERO,
            l2_to_l1_logs_root_hash: B256::ZERO,
            commitment: B256::ZERO,
            // unused
            last_block_timestamp: Some(0),
        },
        batch_info: PendingBatchInfo {
            commit_info: CommitBatchInfo {
                batch_number,
                new_state_commitment: B256::ZERO,
                number_of_layer1_txs: 0,
                number_of_layer2_txs: 0,
                priority_operations_hash: B256::ZERO,
                dependency_roots_rolling_hash: B256::ZERO,
                l2_to_l1_logs_root_hash: B256::ZERO,
                l2_da_commitment_scheme: DACommitmentScheme::BlobsAndPubdataKeccak256,
                da_commitment: B256::ZERO,
                first_block_timestamp: 0,
                first_block_number: Some(batch_number),
                last_block_timestamp: 0,
                last_block_number: Some(batch_number),
                chain_id: 1,
                operator_da_input: vec![],
                sl_chain_id: 2,
            },
            protocol_version: ProtocolSemanticVersion::legacy_genesis_version(),
            upgrade_tx_hash: None,
        },
        chain_address: Address::ZERO,
        blob_sidecar: None,
        first_block_number: batch_number,
        last_block_number: batch_number,
        last_block_hash: None,
        pubdata_mode: PubdataMode::Calldata,
        tx_count: 10,
        computational_native_used: None,
        logs: vec![],
        messages: vec![],
        multichain_root: Default::default(),
        set_sl_chain_id_migration_number: None,
    };

    BatchForSigning::new(batch, data).with_signatures(BatchSignatureData::NotNeeded)
}
