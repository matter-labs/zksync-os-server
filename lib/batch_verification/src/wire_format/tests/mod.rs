use crate::{
    BATCH_VERIFICATION_WIRE_FORMAT_VERSION, BatchVerificationRequest, BatchVerificationResponse,
    BatchVerificationResult,
};
use zksync_os_batch_types::BatchSignature;
use zksync_os_contract_interface::models::CommitBatchInfo;

fn create_sample_request() -> BatchVerificationRequest {
    use alloy::primitives::{Address, B256};

    BatchVerificationRequest {
        batch_number: 42,
        first_block_number: 100,
        last_block_number: 150,
        request_id: 12345,
        commit_data: CommitBatchInfo {
            batch_number: 42,
            new_state_commitment: B256::ZERO,
            number_of_layer1_txs: 5,
            priority_operations_hash: B256::ZERO,
            dependency_roots_rolling_hash: B256::ZERO,
            l2_to_l1_logs_root_hash: B256::ZERO,
            l2_da_validator: Address::ZERO,
            da_commitment: B256::ZERO,
            first_block_timestamp: 1234567890,
            last_block_timestamp: 1234567900,
            chain_id: 270,
            operator_da_input: vec![],
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

// This test generates the binary files for version testing
// Run this once to create the test data files
#[test]
#[ignore]
fn generate_test_data() {
    use std::fs;

    // Generate request v1
    let request = create_sample_request();
    let encoded = request.encode_with_current_version();
    fs::write("src/wire_format/tests/encoded_request_v1.bin", &encoded)
        .expect("Failed to write request v1");

    // Generate response success v1
    let response_success = create_sample_response_success();
    let encoded = response_success.encode_with_version(BATCH_VERIFICATION_WIRE_FORMAT_VERSION);
    fs::write(
        "src/wire_format/tests/encoded_response_success_v1.bin",
        &encoded,
    )
    .expect("Failed to write response success v1");

    // Generate response refused v1
    let response_refused = create_sample_response_refused();
    let encoded = response_refused.encode_with_version(BATCH_VERIFICATION_WIRE_FORMAT_VERSION);
    fs::write(
        "src/wire_format/tests/encoded_response_refused_v1.bin",
        &encoded,
    )
    .expect("Failed to write response refused v1");
}

#[test]
pub fn can_decode_request_v1() {
    let encoded = include_bytes!("encoded_request_v1.bin");
    let decoded = BatchVerificationRequest::decode(encoded, 1);
    let expected = create_sample_request();

    assert_eq!(decoded, expected);
}

#[test]
pub fn can_decode_response_success_v1() {
    let encoded = include_bytes!("encoded_response_success_v1.bin");
    let decoded = BatchVerificationResponse::decode(encoded, 1).unwrap();
    let expected = create_sample_response_success();

    assert_eq!(decoded, expected);
}

#[test]
pub fn can_decode_response_refused_v1() {
    let encoded = include_bytes!("encoded_response_refused_v1.bin");
    let decoded = BatchVerificationResponse::decode(encoded, 1).unwrap();
    let expected = create_sample_response_refused();

    assert_eq!(decoded, expected);
}

#[test]
pub fn request_encode_decode() {
    let original = create_sample_request();
    let encoded = original.clone().encode_with_current_version();
    let decoded = BatchVerificationRequest::decode(&encoded, 1);

    assert_eq!(decoded, original);
}

#[test]
pub fn response_success_encode_decode() {
    let original = create_sample_response_success();
    let encoded = original
        .clone()
        .encode_with_version(BATCH_VERIFICATION_WIRE_FORMAT_VERSION);
    let decoded = BatchVerificationResponse::decode(&encoded, 1).unwrap();

    assert_eq!(decoded, original);
}

#[test]
pub fn response_refused_encode_decode() {
    let original = create_sample_response_refused();
    let encoded = original
        .clone()
        .encode_with_version(BATCH_VERIFICATION_WIRE_FORMAT_VERSION);
    let decoded = BatchVerificationResponse::decode(&encoded, 1).unwrap();

    assert_eq!(decoded, original);
}
