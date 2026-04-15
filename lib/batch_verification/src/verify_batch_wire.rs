use crate::main_node::component::BatchVerificationError;
use alloy::sol_types::SolValue;
use anyhow::anyhow;
use zksync_os_contract_interface::models::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_contract_interface::{IExecutor, IExecutorV29, IExecutorV30};
use zksync_os_l1_sender::batcher_model::BatchForSigning;
use zksync_os_network::VerifyBatch;
use zksync_os_types::PubdataMode;

pub(crate) struct VerificationRequest {
    pub execution_protocol_version: u16,
    pub batch_number: u64,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub pubdata_mode: PubdataMode,
    pub request_id: u64,
    pub commit_data: CommitBatchInfo,
    pub prev_commit_data: StoredBatchInfo,
}

impl TryFrom<VerifyBatch> for VerificationRequest {
    type Error = anyhow::Error;

    fn try_from(request: VerifyBatch) -> Result<Self, Self::Error> {
        let commit_data = decode_commit_data(
            &request.commit_data,
            request.execution_protocol_version,
            request.first_block_number,
            request.last_block_number,
        )?;
        let prev_commit_data = IExecutor::StoredBatchInfo::abi_decode(&request.prev_commit_data)
            .map(StoredBatchInfo::from)
            .map_err(|err| anyhow!("Failed to decode previous commit data: {err}"))?;

        Ok(Self {
            execution_protocol_version: request.execution_protocol_version,
            batch_number: request.batch_number,
            first_block_number: request.first_block_number,
            last_block_number: request.last_block_number,
            pubdata_mode: PubdataMode::from_u8(request.pubdata_mode)
                .ok_or_else(|| anyhow!("Unsupported pubdata mode: {}", request.pubdata_mode))?,
            request_id: request.request_id,
            commit_data,
            prev_commit_data,
        })
    }
}

pub(crate) fn encode_verify_batch_request<E>(
    batch_envelope: &BatchForSigning<E>,
    request_id: u64,
) -> Result<VerifyBatch, BatchVerificationError> {
    let execution_protocol_version = u16::try_from(batch_envelope.batch.protocol_version.minor)
        .map_err(|_| BatchVerificationError::Internal("protocol version overflow".into()))?;
    let commit_data = encode_commit_data(
        batch_envelope.batch.batch_info.commit_info.clone(),
        execution_protocol_version,
    )?;
    let prev_commit_data =
        IExecutor::StoredBatchInfo::from(&batch_envelope.batch.previous_stored_batch_info)
            .abi_encode();

    Ok(VerifyBatch {
        request_id,
        batch_number: batch_envelope.batch_number(),
        first_block_number: batch_envelope.batch.first_block_number,
        last_block_number: batch_envelope.batch.last_block_number,
        pubdata_mode: batch_envelope.batch.pubdata_mode.to_u8(),
        commit_data: commit_data.into(),
        prev_commit_data: prev_commit_data.into(),
        execution_protocol_version,
    })
}

pub(crate) fn normalized_commit_data(
    mut commit_data: CommitBatchInfo,
    execution_protocol_version: u16,
) -> CommitBatchInfo {
    if execution_protocol_version <= 30 {
        commit_data.number_of_layer2_txs = 0;
        commit_data.sl_chain_id = 0;
    }
    commit_data
}

fn decode_commit_data(
    commit_data: &[u8],
    execution_protocol_version: u16,
    first_block_number: u64,
    last_block_number: u64,
) -> anyhow::Result<CommitBatchInfo> {
    Ok(match execution_protocol_version {
        29 => {
            let decoded = IExecutorV29::CommitBatchInfoZKsyncOS::abi_decode(commit_data)
                .map_err(|err| anyhow!("Failed to decode v29 commit data: {err}"))?;
            CommitBatchInfo {
                batch_number: decoded.batchNumber,
                new_state_commitment: decoded.newStateCommitment,
                number_of_layer1_txs: decoded.numberOfLayer1Txs.to::<u64>(),
                number_of_layer2_txs: 0,
                priority_operations_hash: decoded.priorityOperationsHash,
                dependency_roots_rolling_hash: decoded.dependencyRootsRollingHash,
                l2_to_l1_logs_root_hash: decoded.l2LogsTreeRoot,
                l2_da_commitment_scheme:
                    zksync_os_contract_interface::models::DACommitmentScheme::BlobsAndPubdataKeccak256,
                da_commitment: decoded.daCommitment,
                first_block_timestamp: decoded.firstBlockTimestamp,
                first_block_number: Some(first_block_number),
                last_block_timestamp: decoded.lastBlockTimestamp,
                last_block_number: Some(last_block_number),
                chain_id: decoded.chainId.to::<u64>(),
                operator_da_input: decoded.operatorDAInput.as_ref().to_vec(),
                sl_chain_id: 0,
            }
        }
        30 => IExecutorV30::CommitBatchInfoZKsyncOS::abi_decode(commit_data)
            .map(Into::into)
            .map_err(|err| anyhow!("Failed to decode v30 commit data: {err}"))?,
        31 | 32 => IExecutor::CommitBatchInfoZKsyncOS::abi_decode(commit_data)
            .map(Into::into)
            .map_err(|err| anyhow!("Failed to decode v31+ commit data: {err}"))?,
        version => return Err(anyhow!("Unsupported execution protocol version: {version}")),
    })
}

fn encode_commit_data(
    commit_info: CommitBatchInfo,
    protocol_version_minor: u16,
) -> Result<Vec<u8>, BatchVerificationError> {
    Ok(match protocol_version_minor {
        29 => IExecutorV29::CommitBatchInfoZKsyncOS::from(commit_info).abi_encode(),
        30 => IExecutorV30::CommitBatchInfoZKsyncOS::from(commit_info).abi_encode(),
        31 | 32 => IExecutor::CommitBatchInfoZKsyncOS::from(commit_info).abi_encode(),
        version => {
            return Err(BatchVerificationError::Internal(format!(
                "Unsupported protocol version: {version}"
            )));
        }
    })
}
