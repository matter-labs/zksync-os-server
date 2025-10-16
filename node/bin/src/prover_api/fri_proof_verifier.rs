use crate::prover_api::fri_job_manager::SubmitError;
use alloy::primitives::B256;
use execution_utils::{ProgramProof, generate_oracle_data_from_metadata_and_proof_list};
use zk_os_basic_system::system_implementation::system::BatchPublicInput;
use zksync_os_contract_interface::models::StoredBatchInfo;

pub fn verify_fri_proof(
    previous_state_commitment: B256,
    stored_batch_info: StoredBatchInfo,
    proof: ProgramProof,
) -> Result<(), SubmitError> {
    let expected_pi = BatchPublicInput {
        state_before: previous_state_commitment.0.into(),
        state_after: stored_batch_info.state_commitment.0.into(),
        batch_output: stored_batch_info.commitment.0.into(),
    };

    let expected_hash_u32s: [u32; 8] = batch_output_hash_as_register_values(&expected_pi);

    let proof_final_register_values: [u32; 16] = extract_final_register_values(proof);

    tracing::debug!(
        batch_number = stored_batch_info.batch_number,
        "Program final registers: {:?}",
        proof_final_register_values
    );
    tracing::debug!(
        batch_number = stored_batch_info.batch_number,
        ?previous_state_commitment,
        ?stored_batch_info,
        "Expected values for Public Inputs hash: {:?}",
        expected_hash_u32s
    );

    // compare expected_hash_u32s with the last 8 values of proof_final_register_values
    (proof_final_register_values[..8] == expected_hash_u32s)
        .then_some(())
        .ok_or(SubmitError::VerificationFailed)
}

fn batch_output_hash_as_register_values(public_input: &BatchPublicInput) -> [u32; 8] {
    public_input
        .hash()
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("Slice with incorrect length")))
        .collect::<Vec<u32>>()
        .try_into()
        .expect("Hash should be exactly 32 bytes long")
}

fn extract_final_register_values(input_program_proof: ProgramProof) -> [u32; 16] {
    // Once new version of airbender is integrated, these functions should be changed to the ones from execution_utils.
    let (metadata, proof_list) = ProgramProof::to_metadata_and_proof_list(input_program_proof);

    let oracle_data = generate_oracle_data_from_metadata_and_proof_list(&metadata, &proof_list);
    tracing::debug!(
        "Oracle data iterator created with {} items",
        oracle_data.len()
    );

    let it = oracle_data.into_iter();

    full_statement_verifier::verifier_common::prover::nd_source_std::set_iterator(it);

    // Assume that program proof has only recursion proofs.
    tracing::debug!("Running continue recursive");
    assert!(metadata.reduced_proof_count > 0);

    let final_register_values = full_statement_verifier::verify_recursion_layer();

    assert!(
        full_statement_verifier::verifier_common::prover::nd_source_std::try_read_word().is_none(),
        "Expected that all words from CSR were consumed"
    );
    final_register_values
}
