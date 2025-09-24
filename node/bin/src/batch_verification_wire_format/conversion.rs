use super::v1::{BatchVerificationRequestWireFormatV1, BatchVerificationResponseWireFormatV1};
use crate::batch_verification_transport::{BatchVerificationRequest, BatchVerificationResponse};

impl From<BatchVerificationRequestWireFormatV1> for BatchVerificationRequest {
    fn from(value: BatchVerificationRequestWireFormatV1) -> Self {
        let BatchVerificationRequestWireFormatV1 {
            batch_number,
            first_block_number,
            last_block_number,
            request_id,
        } = value;
        Self {
            batch_number,
            first_block_number,
            last_block_number,
            request_id,
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
        } = value;
        Self {
            batch_number,
            first_block_number,
            last_block_number,
            request_id,
        }
    }
}

impl From<BatchVerificationResponseWireFormatV1> for BatchVerificationResponse {
    fn from(value: BatchVerificationResponseWireFormatV1) -> Self {
        let BatchVerificationResponseWireFormatV1 { request, signature } = value;
        Self {
            request: request.into(),
            signature,
        }
    }
}

impl From<BatchVerificationResponse> for BatchVerificationResponseWireFormatV1 {
    fn from(value: BatchVerificationResponse) -> Self {
        let BatchVerificationResponse { request, signature } = value;
        Self {
            request: request.into(),
            signature,
        }
    }
}
