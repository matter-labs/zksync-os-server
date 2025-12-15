use crate::models::{CommitBatchInfo, StoredBatchInfo};
use crate::{IExecutor, IExecutorV29};
use alloy::primitives::Address;
use alloy::sol_types::{SolCall, SolValue};

const V29_ENCODING_VERSION: u8 = 2;
const V30_ENCODING_VERSION: u8 = 3;

pub struct CommitCalldata {
    pub chain_address: Address,
    pub process_from: u64,
    pub process_to: u64,
    pub stored_batch_info: StoredBatchInfo,
    pub commit_batch_info: CommitBatchInfo,
}

impl CommitCalldata {
    pub fn decode(data: &[u8]) -> anyhow::Result<Self> {
        let commit_call = <IExecutor::commitBatchesSharedBridgeCall as SolCall>::abi_decode(data)?;
        let commit_data = commit_call._commitData;
        if commit_data[0] != V30_ENCODING_VERSION {
            anyhow::bail!("unexpected encoding version: {}", commit_data[0]);
        }

        let (stored_batch_info, mut commit_batch_infos) =
            <(
                IExecutor::StoredBatchInfo,
                Vec<IExecutor::CommitBatchInfoZKsyncOS>,
            )>::abi_decode_params(&commit_data[1..])?;
        if commit_batch_infos.len() != 1 {
            anyhow::bail!(
                "unexpected number of committed batch infos: {}",
                commit_batch_infos.len()
            );
        }
        let stored_batch_info = StoredBatchInfo::from(stored_batch_info);
        let commit_batch_info = CommitBatchInfo::from(commit_batch_infos.remove(0));
        Ok(Self {
            chain_address: commit_call._chainAddress,
            process_from: commit_call._processFrom.to(),
            process_to: commit_call._processTo.to(),
            stored_batch_info,
            commit_batch_info,
        })
    }
}

/// This function encodes only the last argument for commitBatchesSharedBridgeCall!
/// Implemented outside of struct to allow only passing necessary arguments
pub fn encode_commit_batch_data(
    prev_batch_info: &StoredBatchInfo,
    commit_info: CommitBatchInfo,
    protocol_version_minor: u64,
) -> Vec<u8> {
    let stored_batch_info = IExecutor::StoredBatchInfo::from(prev_batch_info);
    match protocol_version_minor {
        29 => {
            let commit_batch_info = IExecutorV29::CommitBatchInfoZKsyncOS::from(commit_info);
            tracing::debug!(
                last_batch_hash = ?prev_batch_info.hash(),
                last_batch_number = ?prev_batch_info.batch_number,
                new_batch_number = ?commit_batch_info.batchNumber,
                "preparing commit calldata"
            );
            let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

            // Prefixed by current encoding version as expected by protocol
            [[V29_ENCODING_VERSION].to_vec(), encoded_data].concat()
        }
        // 31 needed for upgrade integration test
        30 | 31 => {
            let commit_batch_info = IExecutor::CommitBatchInfoZKsyncOS::from(commit_info.clone());
            tracing::debug!(
                last_batch_hash = ?prev_batch_info.hash(),
                last_batch_number = ?prev_batch_info.batch_number,
                new_batch_number = ?commit_batch_info.batchNumber,
                "preparing commit calldata"
            );
            let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

            // Prefixed by current encoding version as expected by protocol
            [[V30_ENCODING_VERSION].to_vec(), encoded_data].concat()
        }
        _ => panic!("Unsupported protocol version: {protocol_version_minor}"),
    }
}
