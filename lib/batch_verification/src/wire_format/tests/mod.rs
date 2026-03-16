use crate::{
    BATCH_VERIFICATION_WIRE_FORMAT_VERSION, BatchVerificationRequest, BatchVerificationResponse,
    BatchVerificationResult, wire_format::BatchVerificationCommitInfo,
};
use zksync_os_batch_types::BatchSignature;
use zksync_os_contract_interface::models::{DACommitmentScheme, StoredBatchInfo};
use zksync_os_types::PubdataMode;

fn create_sample_request() -> BatchVerificationRequest {
    use alloy::primitives::B256;

    BatchVerificationRequest {
        batch_number: 42,
        first_block_number: 100,
        last_block_number: 150,
        pubdata_mode: PubdataMode::Blobs,
        request_id: 12345,
        commit_data: BatchVerificationCommitInfo {
            batch_number: 42,
            new_state_commitment: B256::ZERO,
            number_of_layer1_txs: 5,
            priority_operations_hash: B256::ZERO,
            dependency_roots_rolling_hash: B256::ZERO,
            l2_to_l1_logs_root_hash: B256::ZERO,
            l2_da_commitment_scheme: DACommitmentScheme::BlobsZKsyncOS,
            da_commitment: B256::ZERO,
            first_block_timestamp: 1234567890,
            first_block_number: 1,
            last_block_timestamp: 1234567900,
            last_block_number: 2,
            chain_id: 6565,
            operator_da_input: vec![],
        },
        prev_commit_data: StoredBatchInfo {
            batch_number: 41,
            state_commitment: B256::ZERO,
            number_of_layer1_txs: 0,
            priority_operations_hash: B256::ZERO,
            dependency_roots_rolling_hash: B256::ZERO,
            l2_to_l1_logs_root_hash: B256::ZERO,
            commitment: B256::ZERO,
            // unused
            last_block_timestamp: Some(0),
        },
    }
}

fn create_sample_response_success() -> BatchVerificationResponse {
    BatchVerificationResponse {
        request_id: 12345,
        batch_number: 42,
        result: BatchVerificationResult::Success(
            BatchSignature::from_raw_array(&[42u8; 65]).unwrap(),
        ),
    }
}

fn create_sample_response_refused() -> BatchVerificationResponse {
    BatchVerificationResponse {
        request_id: 12345,
        batch_number: 42,
        result: BatchVerificationResult::Refused("Test refusal reason".to_string()),
    }
}

// Regenerates the hardcoded wire-format fixture files for the active transport version.
//
// Use this helper only when intentionally changing the active batch-verification wire format.
// The filenames are derived from `BATCH_VERIFICATION_WIRE_FORMAT_VERSION`, so this test writes
// fixtures only for the currently active transport format. If we later need to regenerate
// historical fixtures for older versions after the payload evolves, those should be produced
// from the frozen versioned wire DTOs instead of this helper.
#[test]
#[ignore]
fn generate_test_data() {
    use std::fs;

    let version = BATCH_VERIFICATION_WIRE_FORMAT_VERSION;

    let request = create_sample_request();
    let encoded = request.encode();
    fs::write(
        format!("src/wire_format/tests/encoded_request_v{version}.bin"),
        &encoded,
    )
    .unwrap_or_else(|_| panic!("Failed to write request v{version}"));

    let response_success = create_sample_response_success();
    let encoded = response_success.encode();
    fs::write(
        format!("src/wire_format/tests/encoded_response_success_v{version}.bin"),
        &encoded,
    )
    .unwrap_or_else(|_| panic!("Failed to write response success v{version}"));

    let response_refused = create_sample_response_refused();
    let encoded = response_refused.encode();
    fs::write(
        format!("src/wire_format/tests/encoded_response_refused_v{version}.bin"),
        &encoded,
    )
    .unwrap_or_else(|_| panic!("Failed to write response refused v{version}"));
}

#[test]
pub fn can_decode_request_v1() {
    let encoded = include_bytes!("encoded_request_v1.bin");
    let decoded = BatchVerificationRequest::decode(encoded);
    let expected = create_sample_request();

    assert_eq!(decoded, expected);
}

#[test]
pub fn can_decode_response_success_v1() {
    let encoded = include_bytes!("encoded_response_success_v1.bin");
    let decoded = BatchVerificationResponse::decode(encoded).unwrap();
    let expected = create_sample_response_success();

    assert_eq!(decoded, expected);
}

#[test]
pub fn can_decode_response_refused_v1() {
    let encoded = include_bytes!("encoded_response_refused_v1.bin");
    let decoded = BatchVerificationResponse::decode(encoded).unwrap();
    let expected = create_sample_response_refused();

    assert_eq!(decoded, expected);
}

#[test]
pub fn request_encode_decode() {
    let original = create_sample_request();
    let encoded = original.clone().encode();
    let decoded = BatchVerificationRequest::decode(&encoded);

    assert_eq!(decoded, original);
}

#[test]
pub fn response_success_encode_decode() {
    let original = create_sample_response_success();
    let encoded = original.clone().encode();
    let decoded = BatchVerificationResponse::decode(&encoded).unwrap();

    assert_eq!(decoded, original);
}

#[test]
pub fn response_refused_encode_decode() {
    let original = create_sample_response_refused();
    let encoded = original.clone().encode();
    let decoded = BatchVerificationResponse::decode(&encoded).unwrap();

    assert_eq!(decoded, original);
}

// These fixture checks lock in the historical `v1` wire format.
//
// Batch verification still needs to interoperate with nodes that speak the
// legacy `v1` protocol, so the current encoder must continue to produce
// byte-for-byte identical `v1` payloads for requests and responses.
//
// If these tests fail, we have changed the serialized `v1` format and may
// have broken compatibility with older nodes.
#[test]
pub fn current_request_encoding_matches_v1_fixture() {
    let original = create_sample_request();
    let encoded = original.encode();
    let expected = include_bytes!("encoded_request_v1.bin");

    assert_eq!(encoded.as_slice(), expected);
}

#[test]
pub fn current_response_success_encoding_matches_v1_fixture() {
    let original = create_sample_response_success();
    let encoded = original.encode();
    let expected = include_bytes!("encoded_response_success_v1.bin");

    assert_eq!(encoded.as_slice(), expected);
}

#[test]
pub fn current_response_refused_encoding_matches_v1_fixture() {
    let original = create_sample_response_refused();
    let encoded = original.encode();
    let expected = include_bytes!("encoded_response_refused_v1.bin");

    assert_eq!(encoded.as_slice(), expected);
}
