#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch_verification_transport::{
        BatchVerificationRequest, BatchVerificationResponse,
    };

    #[test]
    fn test_batch_verification_request_roundtrip() {
        let original = BatchVerificationRequest {
            batch_number: 42,
            first_block_number: 100,
            last_block_number: 150,
            request_id: 12345,
        };

        let encoded = original.clone().encode_with_current_version();
        let decoded =
            BatchVerificationRequest::decode(&encoded, BATCH_VERIFICATION_WIRE_FORMAT_VERSION);

        assert_eq!(original.batch_number, decoded.batch_number);
        assert_eq!(original.first_block_number, decoded.first_block_number);
        assert_eq!(original.last_block_number, decoded.last_block_number);
        assert_eq!(original.request_id, decoded.request_id);
    }

    #[test]
    fn test_batch_verification_response_roundtrip() {
        let request = BatchVerificationRequest {
            batch_number: 42,
            first_block_number: 100,
            last_block_number: 150,
            request_id: 12345,
        };

        let original = BatchVerificationResponse {
            request,
            signature: vec![1, 2, 3, 4, 5],
        };

        let encoded = original.clone().encode_with_current_version();
        let decoded =
            BatchVerificationResponse::decode(&encoded, BATCH_VERIFICATION_WIRE_FORMAT_VERSION);

        assert_eq!(original.request.batch_number, decoded.request.batch_number);
        assert_eq!(
            original.request.first_block_number,
            decoded.request.first_block_number
        );
        assert_eq!(
            original.request.last_block_number,
            decoded.request.last_block_number
        );
        assert_eq!(original.request.request_id, decoded.request.request_id);
        assert_eq!(original.signature, decoded.signature);
    }
}
