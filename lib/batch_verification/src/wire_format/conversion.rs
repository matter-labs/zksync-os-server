use super::v1::{BatchVerificationRequestWireFormatV1, BatchVerificationResponseWireFormatV1};
use crate::{BatchVerificationRequest, BatchVerificationResponse};
use alloy::sol_types::SolValue;
use zksync_os_contract_interface::{IExecutor::CommitBatchInfoZKsyncOS, models::CommitBatchInfo};

impl From<BatchVerificationRequestWireFormatV1> for BatchVerificationRequest {
    fn from(value: BatchVerificationRequestWireFormatV1) -> Self {
        let BatchVerificationRequestWireFormatV1 {
            batch_number,
            first_block_number,
            last_block_number,
            request_id,
            commit_data,
        } = value;
        let decoded_commit_data_alloy = CommitBatchInfoZKsyncOS::abi_decode(&commit_data)
            .expect("Failed to decode commit data");
        let decoded_commit_data = CommitBatchInfo::from(decoded_commit_data_alloy);
        Self {
            batch_number,
            first_block_number,
            last_block_number,
            request_id,
            commit_data: decoded_commit_data,
        }
    }
}

impl From<BatchVerificationRequest> for BatchVerificationRequestWireFormatV1 {
    fn from(value: BatchVerificationRequest) -> Self {
        let BatchVerificationRequest {
            batch_number,
            first_block_number,
            last_block_number,
            request_id,
            commit_data,
        } = value;
        let commit_data_alloy = CommitBatchInfoZKsyncOS::from(commit_data);
        let encoded_commit_data = commit_data_alloy.abi_encode();
        Self {
            batch_number,
            first_block_number,
            last_block_number,
            request_id,
            commit_data: encoded_commit_data,
        }
    }
}

impl From<BatchVerificationResponseWireFormatV1> for BatchVerificationResponse {
    fn from(value: BatchVerificationResponseWireFormatV1) -> Self {
        let BatchVerificationResponseWireFormatV1 {
            request_id,
            signature,
        } = value;
        Self {
            request_id,
            signature,
        }
    }
}

impl From<BatchVerificationResponse> for BatchVerificationResponseWireFormatV1 {
    fn from(value: BatchVerificationResponse) -> Self {
        let BatchVerificationResponse {
            request_id,
            signature,
        } = value;
        Self {
            request_id,
            signature,
        }
    }
}
