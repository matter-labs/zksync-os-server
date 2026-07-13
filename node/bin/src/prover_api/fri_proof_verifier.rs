use crate::prover_api::fri_job_manager::SubmitError;
use alloy::primitives::{B256, keccak256};
use zksync_os_batch_types::PendingBatchInfo;
use zksync_os_contract_interface::models::StoredBatchInfo;

#[derive(Debug)]
struct BatchPublicInput {
    /// State commitment before the batch.
    /// It should commit for everything needed for trustless execution(state, block number, hashes, etc).
    pub state_before: B256,
    /// State commitment after the batch.
    pub state_after: B256,
    /// Batch output to be opened on the settlement layer, needed to process DA, l1 <> l2 messaging, validate inputs.
    pub batch_output: B256,
}

impl BatchPublicInput {
    ///
    /// Calculate keccak256 hash of public input
    ///
    pub fn hash(&self) -> B256 {
        keccak256([self.state_before.0, self.state_after.0, self.batch_output.0].concat())
    }
}

pub fn verify_fri_proof(
    previous_state_commitment: B256,
    stored_batch_info: StoredBatchInfo,
    proof: execution_utils::ProgramProof,
) -> Result<(), SubmitError> {
    let expected_pi = BatchPublicInput {
        state_before: previous_state_commitment,
        state_after: stored_batch_info.state_commitment,
        batch_output: stored_batch_info.commitment,
    };

    let expected_hash_u32s: [u32; 8] = batch_output_hash_as_register_values(&expected_pi);

    // The statement verifier asserts (panics) on malformed proofs; catch it so a bad
    // proof is reported - and persisted for debugging - as a verification failure.
    let proof_final_register_values: [u32; 16] =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            extract_final_register_values(proof)
        }))
        .map_err(|_| {
            tracing::warn!(
                batch_number = stored_batch_info.batch_number,
                "proof verifier panicked on a malformed proof"
            );
            SubmitError::FriProofVerificationError {
                expected_hash_u32s,
                // The verifier failed before producing register values.
                proof_final_register_values: [0u32; 16],
            }
        })?;

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
        .ok_or(SubmitError::FriProofVerificationError {
            expected_hash_u32s,
            proof_final_register_values,
        })
}

/// Verifies a V8 FRI proof (zksync-os 0.4.0 / airbender unrolled prover stack).
///
/// V8 provers submit an `UnrolledProgramProof` recursed up to the *unified* layer. The unified
/// recursion program is app-independent and embedded in `execution_utils_0_4_0`, so verification
/// needs no app binary: we run the native unified-layer statement verifier to trustlessly extract
/// the final register values and compare them against the expected batch public input hash.
///
/// Unlike the pre-V8 flow, the V8 batch public input is
/// `keccak(state_before || state_after || chain_config_hash || batch_output)`, where
/// `batch_output` uses the 0.4.0 layout without the leading chain id
/// (see [`PendingBatchInfo::v8_batch_output_hash`]).
pub fn verify_fri_proof_v8(
    previous_state_commitment: B256,
    batch_info: &PendingBatchInfo,
    proof: execution_utils_0_4_0::unrolled::UnrolledProgramProof,
) -> Result<(), SubmitError> {
    let batch_number = batch_info.commit_info.batch_number;
    let chain_config_hash = zksync_os_native_pig::v8_chain_config_hash(
        batch_info.commit_info.chain_id,
    )
    .map_err(|err| SubmitError::Other(format!("cannot compute V8 chain config hash: {err:#}")))?;
    let expected_pi_hash = keccak256(
        [
            previous_state_commitment.0,
            batch_info.commit_info.new_state_commitment.0,
            chain_config_hash.0,
            batch_info.v8_batch_output_hash().0,
        ]
        .concat(),
    );
    let expected_hash_u32s: [u32; 8] = hash_as_register_values(expected_pi_hash);

    // The unified-layer verifier returns Err on invalid proofs, but its internals can
    // still assert (panic) on malformed input; catch it so a bad proof is reported -
    // and persisted for debugging - as a verification failure.
    let proof_final_register_values: [u32; 16] =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            extract_final_register_values_v8(&proof)
        }))
        .unwrap_or_else(|_| {
            tracing::warn!(
                batch_number,
                "V8 unified-layer verifier panicked on a malformed proof"
            );
            Err(())
        })
        .map_err(|()| {
            tracing::warn!(
                batch_number,
                "V8 unified-layer proof failed cryptographic verification"
            );
            SubmitError::FriProofVerificationError {
                expected_hash_u32s,
                // The verifier failed before producing register values.
                proof_final_register_values: [0u32; 16],
            }
        })?;

    tracing::debug!(
        batch_number,
        "V8 program final registers: {:?}",
        proof_final_register_values
    );
    tracing::debug!(
        batch_number,
        ?previous_state_commitment,
        ?chain_config_hash,
        "Expected values for Public Inputs hash: {:?}",
        expected_hash_u32s
    );

    (proof_final_register_values[..8] == expected_hash_u32s)
        .then_some(())
        .ok_or(SubmitError::FriProofVerificationError {
            expected_hash_u32s,
            proof_final_register_values,
        })
}

/// Runs the airbender-73d69b5 unified-layer verifier over the proof and returns the final
/// register values. The unified-layer setup and circuit layouts are derived from the embedded
/// recursion program once and cached.
fn extract_final_register_values_v8(
    proof: &execution_utils_0_4_0::unrolled::UnrolledProgramProof,
) -> Result<[u32; 16], ()> {
    use execution_utils_0_4_0::setups::{
        binary_u8_to_u32, get_unified_circuit_artifact_for_machine_type,
        pad_bytecode_bytes_for_proving, pad_bytecode_for_proving,
    };
    use execution_utils_0_4_0::unified_circuit::{
        compute_unified_setup_for_machine_configuration, verify_proof_in_unified_layer,
    };
    use execution_utils_0_4_0::unrolled::UnrolledProgramSetup;
    use execution_utils_0_4_0::verifier_binaries::recursion_artifact;
    use execution_utils_0_4_0::{RecursionArtifact, RecursionLayer};
    use riscv_transpiler_0_4_0::cycle::IWithoutByteAccessIsaConfigWithDelegation;
    use verifier_common_0_4_0::SecurityModel;

    static UNIFIED_LEVEL_DATA: std::sync::OnceLock<(
        UnrolledProgramSetup,
        execution_utils_0_4_0::setups::CompiledCircuitsSet,
    )> = std::sync::OnceLock::new();

    const SECURITY: SecurityModel = SecurityModel::Security80;

    let (setup, layouts) = UNIFIED_LEVEL_DATA.get_or_init(|| {
        let binary = recursion_artifact(SECURITY, RecursionLayer::Unified, RecursionArtifact::Bin);
        let text = recursion_artifact(SECURITY, RecursionLayer::Unified, RecursionArtifact::Txt);

        let mut padded_bin_bytes = binary.to_vec();
        let mut padded_text_bytes = text.to_vec();
        pad_bytecode_bytes_for_proving(&mut padded_bin_bytes);
        pad_bytecode_bytes_for_proving(&mut padded_text_bytes);

        let mut padded_bin_u32 = binary_u8_to_u32(binary);
        pad_bytecode_for_proving(&mut padded_bin_u32);

        let setup = compute_unified_setup_for_machine_configuration::<
            IWithoutByteAccessIsaConfigWithDelegation,
        >(&padded_bin_bytes, &padded_text_bytes);
        let layouts = get_unified_circuit_artifact_for_machine_type::<
            IWithoutByteAccessIsaConfigWithDelegation,
        >(&padded_bin_u32);
        (setup, layouts)
    });

    verify_proof_in_unified_layer(proof, setup, layouts, false, SECURITY)
}

fn batch_output_hash_as_register_values(public_input: &BatchPublicInput) -> [u32; 8] {
    hash_as_register_values(public_input.hash())
}

fn hash_as_register_values(hash: B256) -> [u32; 8] {
    hash.0
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("Slice with incorrect length")))
        .collect::<Vec<u32>>()
        .try_into()
        .expect("Hash should be exactly 32 bytes long")
}

fn extract_final_register_values(input_program_proof: execution_utils::ProgramProof) -> [u32; 16] {
    // Once new version of airbender is integrated, these functions should be changed to the ones from execution_utils.
    let (metadata, proof_list) =
        execution_utils::ProgramProof::to_metadata_and_proof_list(input_program_proof);

    let oracle_data =
        execution_utils::generate_oracle_data_from_metadata_and_proof_list(&metadata, &proof_list);
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

#[cfg(test)]
mod tests {
    use super::extract_final_register_values_v8;

    /// Smoke test for the V8 unified-layer verifier lane against a real proof produced by the
    /// airbender CLI (rev 73d69b5) with `--target recursion-unified`.
    ///
    /// Run manually:
    ///   V8_PROOF_ARTIFACT_JSON=/path/to/proof.json \
    ///     cargo test -p zksync_os_server --release v8_unified_layer_verifies_cli_proof -- --ignored
    #[test]
    #[ignore = "needs a locally produced V8 proof artifact"]
    fn v8_unified_layer_verifies_cli_proof() {
        let path = std::env::var("V8_PROOF_ARTIFACT_JSON")
            .expect("set V8_PROOF_ARTIFACT_JSON to an airbender CLI proof.json");
        let artifact: serde_json::Value =
            serde_json::from_reader(std::fs::File::open(&path).expect("cannot open proof file"))
                .expect("proof file is not valid JSON");
        // The CLI stores `ProofArtifact { proof: UnrolledProgramProof, .. }`.
        let proof: execution_utils_0_4_0::unrolled::UnrolledProgramProof =
            serde_json::from_value(artifact["proof"].clone())
                .expect("artifact has no deserializable `proof` field");

        let registers = extract_final_register_values_v8(&proof)
            .expect("V8 unified-layer proof failed cryptographic verification");
        println!("final register values: {registers:?}");
    }
}
