use super::v1::{BatchVerificationRequestWireFormatV1, BatchVerificationResponseWireFormatV1};
use crate::{
    BatchVerificationRequest, BatchVerificationResponse, response::BatchVerificationResult,
    wire_format::v1::BatchVerificationResponseResultWireFormatV1,
};
use alloy::sol_types::SolValue;
use zksync_os_batch_types::BatchSignature;
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

impl TryFrom<BatchVerificationResponseWireFormatV1> for BatchVerificationResponse {
    type Error = anyhow::Error;

    fn try_from(value: BatchVerificationResponseWireFormatV1) -> Result<Self, Self::Error> {
        let BatchVerificationResponseWireFormatV1 {
            request_id,
            batch_number,
            result: wire_result,
        } = value;
        let result = match wire_result {
            BatchVerificationResponseResultWireFormatV1::Success(bytes) => {
                BatchVerificationResult::Success(BatchSignature::from_raw_array(&bytes)?)
            }
            BatchVerificationResponseResultWireFormatV1::Refused(reason) => {
                BatchVerificationResult::Refused(reason)
            }
        };
        Ok(Self {
            request_id,
            batch_number,
            result,
        })
    }
}

impl From<BatchVerificationResponse> for BatchVerificationResponseWireFormatV1 {
    fn from(value: BatchVerificationResponse) -> Self {
        let BatchVerificationResponse {
            request_id,
            batch_number,
            result,
        } = value;
        let wire_result = match result {
            BatchVerificationResult::Success(signature) => {
                BatchVerificationResponseResultWireFormatV1::Success(signature.into_raw())
            }
            BatchVerificationResult::Refused(reason) => {
                BatchVerificationResponseResultWireFormatV1::Refused(reason)
            }
        };
        Self {
            request_id,
            batch_number,
            result: wire_result,
        }
    }
}
