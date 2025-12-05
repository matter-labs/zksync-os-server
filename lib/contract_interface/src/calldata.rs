use crate::IExecutor;
use crate::models::{CommitBatchInfo, StoredBatchInfo};
use alloy::primitives::Address;
use alloy::sol_types::{SolCall, SolValue};

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
