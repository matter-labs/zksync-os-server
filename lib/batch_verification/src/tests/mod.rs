use alloy::primitives::{Address, B256};
/// This module is for sharing various testing utilities and helpers.
use tokio::sync::watch;
use zksync_os_batch_types::BatchInfo;
use zksync_os_contract_interface::models::{CommitBatchInfo, DACommitmentScheme, StoredBatchInfo};
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, BatchMetadata, MissingSignature};
use zksync_os_storage_api::{FinalityStatus, ReadFinality};
use zksync_os_types::ProtocolSemanticVersion;

pub struct DummyFinality {
    status: FinalityStatus,
    rx: watch::Receiver<FinalityStatus>,
}

impl DummyFinality {
    pub fn zero() -> Self {
        let status = FinalityStatus {
            last_committed_block: 0,
            last_committed_batch: 0,
            last_executed_block: 0,
            last_executed_batch: 0,
        };
        let (tx, rx) = watch::channel(status.clone());
        let _ = tx;
        Self { status, rx }
    }
}

impl ReadFinality for DummyFinality {
    fn get_finality_status(&self) -> FinalityStatus {
        self.status.clone()
    }

    fn subscribe(&self) -> watch::Receiver<FinalityStatus> {
        self.rx.clone()
    }
}

pub fn dummy_commit_batch_info(batch_number: u64, from: u64, to: u64) -> CommitBatchInfo {
    CommitBatchInfo {
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
        first_block_number: Some(from),
        last_block_timestamp: 0,
        last_block_number: Some(to),
        chain_id: 270,
        operator_da_input: Vec::new(),
        sl_chain_id: 123,
    }
}

pub fn dummy_batch_metadata(batch_number: u64, from: u64, to: u64) -> BatchMetadata {
    BatchMetadata {
        previous_stored_batch_info: StoredBatchInfo {
            batch_number: batch_number - 1,
            state_commitment: B256::ZERO,
            number_of_layer1_txs: 0,
            priority_operations_hash: B256::ZERO,
            dependency_roots_rolling_hash: B256::ZERO,
            l2_to_l1_logs_root_hash: B256::ZERO,
            commitment: B256::ZERO,
            // unused
            last_block_timestamp: Some(0),
        },
        batch_info: BatchInfo {
            commit_info: dummy_commit_batch_info(batch_number, from, to),
            chain_address: Address::ZERO,
            upgrade_tx_hash: None,
            blob_sidecar: None,
        },
        first_block_number: from,
        last_block_number: to,
        pubdata_mode: zksync_os_types::PubdataMode::Calldata,
        tx_count: 0,
        execution_version: 1,
        protocol_version: ProtocolSemanticVersion::legacy_genesis_version(),
        computational_native_used: None,
        logs: vec![],
        messages: vec![],
        multichain_root: Default::default(),
    }
}

pub fn dummy_batch_envelope(
    batch_number: u64,
    from: u64,
    to: u64,
) -> BatchEnvelope<(), MissingSignature> {
    BatchEnvelope::new(dummy_batch_metadata(batch_number, from, to), ())
}
