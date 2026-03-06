use crate::models::{CommitBatchInfo, StoredBatchInfo};
use crate::{IExecutor, IExecutorV29, IExecutorV30, IMultisigCommitter};
use alloy::primitives::Address;
use alloy::sol_types::{SolCall, SolValue};

const V29_ENCODING_VERSION: u8 = 2;
const V30_ENCODING_VERSION: u8 = 3;
const V31_ENCODING_VERSION: u8 = 4;

pub struct CommitCalldata {
    pub chain_address: Address,
    pub process_from: u64,
    pub process_to: u64,
    pub stored_batch_info: StoredBatchInfo,
    pub commit_batch_info: CommitBatchInfo,
}

impl CommitCalldata {
    pub fn decode(data: &[u8]) -> anyhow::Result<Self> {
        // Check if data is long enough to contain a selector
        if data.len() < 4 {
            anyhow::bail!("data too short to contain function selector");
        }

        // Extract the 4-byte function selector
        let selector = &data[0..4];

        let (chain_address, process_from, process_to, commit_data) =
            if selector == IExecutor::commitBatchesSharedBridgeCall::SELECTOR {
                let commit_call =
                    <IExecutor::commitBatchesSharedBridgeCall as SolCall>::abi_decode(data)?;
                (
                    commit_call._chainAddress,
                    commit_call._processFrom.to(),
                    commit_call._processTo.to(),
                    commit_call._commitData,
                )
            } else if selector == IMultisigCommitter::commitBatchesMultisigCall::SELECTOR {
                let commit_call =
                    <IMultisigCommitter::commitBatchesMultisigCall as SolCall>::abi_decode(data)?;
                (
                    commit_call.chainAddress,
                    commit_call._processBatchFrom.to(),
                    commit_call._processBatchTo.to(),
                    commit_call._batchData,
                )
            } else {
                anyhow::bail!(
                    "unknown function selector: 0x{}",
                    alloy::hex::encode(selector)
                );
            };

        if commit_data[0] != V30_ENCODING_VERSION && commit_data[0] != V31_ENCODING_VERSION {
            anyhow::bail!("unexpected encoding version: {}", commit_data[0]);
        }

        let (stored_batch_info, commit_batch_info) = match commit_data[0] {
            V30_ENCODING_VERSION => {
                let (stored_batch_info, mut commit_batch_infos) =
                    <(
                        IExecutor::StoredBatchInfo,
                        Vec<IExecutorV30::CommitBatchInfoZKsyncOS>,
                    )>::abi_decode_params(&commit_data[1..])?;
                if commit_batch_infos.len() != 1 {
                    anyhow::bail!(
                        "unexpected number of committed batch infos: {}",
                        commit_batch_infos.len()
                    );
                }
                (
                    StoredBatchInfo::from(stored_batch_info),
                    CommitBatchInfo::from(commit_batch_infos.remove(0)),
                )
            }
            V31_ENCODING_VERSION => {
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
                (
                    StoredBatchInfo::from(stored_batch_info),
                    CommitBatchInfo::from(commit_batch_infos.remove(0)),
                )
            }
            _ => unreachable!("encoding version pre-validated"),
        };
        Ok(Self {
            chain_address,
            process_from,
            process_to,
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
        30 => {
            let commit_batch_info =
                IExecutorV30::CommitBatchInfoZKsyncOS::from(commit_info.clone());
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
        31 | 32 => {
            let commit_batch_info = IExecutor::CommitBatchInfoZKsyncOS::from(commit_info.clone());
            tracing::debug!(
                last_batch_hash = ?prev_batch_info.hash(),
                last_batch_number = ?prev_batch_info.batch_number,
                new_batch_number = ?commit_batch_info.batchNumber,
                "preparing commit calldata"
            );
            let encoded_data = (stored_batch_info, vec![commit_batch_info]).abi_encode_params();

            // Prefixed by current encoding version as expected by protocol
            [[V31_ENCODING_VERSION].to_vec(), encoded_data].concat()
        }
        _ => panic!("Unsupported protocol version: {protocol_version_minor}"),
    }
}
