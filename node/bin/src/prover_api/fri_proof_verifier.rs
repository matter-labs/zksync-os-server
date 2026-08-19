use crate::prover_api::fri_job_manager::SubmitError;
use alloy::primitives::{B256, keccak256};
use zksync_os_batch_types::batcher_model::BatchMetadata;
use zksync_os_types::{FriPublicInputLayout, ProvingStackConfiguration};

/// Expected batch public-input hash, as the final register values a valid FRI proof of this
/// batch must expose.
///
/// The ZKsync OS 0.3 public input is `keccak(state_before || state_after || batch_output)`.
/// The ZKsync OS 0.4 batch public input is
/// `keccak(state_before || state_after || chain_config_hash || batch_output)`, where
/// `batch_output` uses the 0.4.0 layout without the leading chain id
/// (see [`PendingBatchInfo::v32_batch_output_hash`](zksync_os_batch_types::PendingBatchInfo)).
pub fn expected_public_input_registers(
    proving_config: &ProvingStackConfiguration,
    batch_metadata: &BatchMetadata,
) -> Result<[u32; 8], SubmitError> {
    let state_before = batch_metadata.previous_stored_batch_info.state_commitment;
    let hash = match proving_config.fri.public_input_layout {
        FriPublicInputLayout::StateTransitionChainConfigAndBatchOutput => {
            let batch_info = &batch_metadata.batch_info;
            let chain_config_hash =
                zksync_os_native_pig::v32_chain_config_hash(batch_info.commit_info.chain_id)
                    .map_err(|err| {
                        SubmitError::Other(format!(
                            "cannot compute chain config hash for {}: {err:#}",
                            proving_config.name
                        ))
                    })?;
            keccak256(
                [
                    state_before.0,
                    batch_info.commit_info.new_state_commitment.0,
                    chain_config_hash.0,
                    batch_info.v32_batch_output_hash().0,
                ]
                .concat(),
            )
        }
        FriPublicInputLayout::StateTransitionAndBatchOutput => {
            let stored = batch_metadata.batch_info.clone().into_stored();
            keccak256(
                [
                    state_before.0,
                    stored.state_commitment.0,
                    stored.commitment.0,
                ]
                .concat(),
            )
        }
    };
    Ok(hash_as_register_values(hash))
}

/// Verifies an Airbender program proof against the expected public input.
pub fn verify_fri_proof(
    expected_hash_u32s: [u32; 8],
    proof: execution_utils_prev::ProgramProof,
    batch_number: u64,
) -> Result<(), SubmitError> {
    // The statement verifier asserts (panics) on malformed proofs; catch it so a bad
    // proof is reported - and persisted for debugging - as a verification failure.
    let proof_final_register_values: [u32; 16] =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            extract_final_register_values(proof)
        }))
        .map_err(|_| {
            tracing::warn!(batch_number, "proof verifier panicked on a malformed proof");
            SubmitError::FriProofVerificationError {
                expected_hash_u32s,
                // The verifier failed before producing register values.
                proof_final_register_values: [0u32; 16],
            }
        })?;

    check_public_input(expected_hash_u32s, proof_final_register_values)
}

/// Verifies an Airbender unified-layer FRI proof.
///
/// Provers for this stack submit an `UnrolledProgramProof` recursed up to the unified layer. The
/// recursion program is app-independent and embedded in `execution_utils`, so verification
/// needs no app binary: we run the native unified-layer statement verifier to trustlessly extract
/// the final register values, check that the proof's recursion chain is rooted in the registered batch
/// program (registers `[8..16]`), and compare registers `[..8]` against the expected batch
/// public input hash.
pub fn verify_fri_proof_unified(
    expected_hash_u32s: [u32; 8],
    proof: &execution_utils::unrolled::UnrolledProgramProof,
    batch_number: u64,
    app_end_params: &[u32; 8],
) -> Result<(), SubmitError> {
    // Cheap consistency check of the carried chain fields (mirrors the airbender CLI's
    // `validate_recursion_chain`).
    unified_verifier::validate_recursion_chain(proof).map_err(|msg| {
        tracing::warn!(
            batch_number,
            msg,
            "unified-layer proof carries an invalid recursion chain"
        );
        SubmitError::Other(format!(
            "invalid unified-layer proof recursion chain: {msg}"
        ))
    })?;

    // The unified-layer verifier returns Err on invalid proofs, but its internals can
    // still assert (panic) on malformed input; catch it so a bad proof is reported -
    // and persisted for debugging - as a verification failure.
    let proof_final_register_values: [u32; 16] =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            unified_verifier::verify(proof)
        }))
        .unwrap_or_else(|_| {
            tracing::warn!(
                batch_number,
                "unified-layer verifier panicked on a malformed proof"
            );
            Err(())
        })
        .map_err(|()| SubmitError::FriProofVerificationError {
            expected_hash_u32s,
            // The verifier failed before producing register values.
            proof_final_register_values: [0u32; 16],
        })?;

    // Bind the proof to the registered batch program. The unified-layer verifier is app-independent
    // and authenticates the recursion chain it actually proved in registers [8..16]; only a
    // chain rooted in the registered program's end params is acceptable — otherwise any guest
    // program exposing the right public-input hash would pass.
    let expected_chain = unified_verifier::unified_level_data().expected_chain(app_end_params);
    if proof_final_register_values[8..16] != expected_chain {
        tracing::warn!(
            batch_number,
            ?expected_chain,
            actual_chain = ?&proof_final_register_values[8..16],
            "unified-layer proof proves a different program (recursion chain mismatch)"
        );
        return Err(SubmitError::FriProofVerificationError {
            expected_hash_u32s,
            proof_final_register_values,
        });
    }

    check_public_input(expected_hash_u32s, proof_final_register_values)
}

/// Compares the expected public-input hash with the first 8 final register values.
fn check_public_input(
    expected_hash_u32s: [u32; 8],
    proof_final_register_values: [u32; 16],
) -> Result<(), SubmitError> {
    (proof_final_register_values[..8] == expected_hash_u32s)
        .then_some(())
        .ok_or(SubmitError::FriProofVerificationError {
            expected_hash_u32s,
            proof_final_register_values,
        })
}

/// Unified-layer verification internals for the pinned Airbender verifier.
mod unified_verifier {
    use execution_utils::setups::{
        CompiledCircuitsSet, binary_u8_to_u32, get_unified_circuit_artifact_for_machine_type,
        pad_bytecode_bytes_for_proving, pad_bytecode_for_proving,
    };
    use execution_utils::unified_circuit::{
        compute_unified_setup_for_machine_configuration, verify_proof_in_unified_layer,
    };
    use execution_utils::unrolled::{
        UnrolledProgramProof, UnrolledProgramSetup, compute_setup_for_machine_configuration,
    };
    use execution_utils::verifier_binaries::recursion_artifact;
    use execution_utils::{RecursionArtifact, RecursionLayer};
    use riscv_transpiler::cycle::IWithoutByteAccessIsaConfigWithDelegation;
    use verifier_common::SecurityModel;
    use verifier_common::transcript::Blake2sBufferingTranscript;

    /// v32 proves at 100-bit; this selects the recursion verifier binaries the chain below is
    /// continued through, so it must match the prover's `PROVING_SECURITY_LEVEL`.
    const SECURITY: SecurityModel = SecurityModel::Security100;

    pub(super) struct UnifiedLevelData {
        setup: UnrolledProgramSetup,
        unrolled_setup: UnrolledProgramSetup,
        layouts: CompiledCircuitsSet,
    }

    impl UnifiedLevelData {
        /// Reconstructs the chain authenticated in final registers `[8..16]`.
        pub(super) fn expected_chain(&self, app_end_params: &[u32; 8]) -> [u32; 8] {
            let (base_chain, base_preimage) =
                UnrolledProgramSetup::begin_recursion_chain(app_end_params);
            let (unrolled_chain, unrolled_preimage) =
                UnrolledProgramSetup::continue_recursion_chain(
                    &self.unrolled_setup.end_params,
                    &base_chain,
                    &base_preimage,
                );
            UnrolledProgramSetup::continue_recursion_chain(
                &self.setup.end_params,
                &unrolled_chain,
                &unrolled_preimage,
            )
            .0
        }
    }

    fn padded_bytes(bytes: &[u8]) -> Vec<u8> {
        let mut padded = bytes.to_vec();
        pad_bytecode_bytes_for_proving(&mut padded);
        padded
    }

    /// The unified-layer setup and circuit layouts are derived from the embedded recursion
    /// programs once and cached. Application binding remains per registered stack.
    pub(super) fn unified_level_data() -> &'static UnifiedLevelData {
        static DATA: std::sync::OnceLock<UnifiedLevelData> = std::sync::OnceLock::new();
        DATA.get_or_init(|| {
            let unified_bin =
                recursion_artifact(SECURITY, RecursionLayer::Unified, RecursionArtifact::Bin);
            let unified_text =
                recursion_artifact(SECURITY, RecursionLayer::Unified, RecursionArtifact::Txt);
            let setup = compute_unified_setup_for_machine_configuration::<
                IWithoutByteAccessIsaConfigWithDelegation,
            >(&padded_bytes(unified_bin), &padded_bytes(unified_text));
            let mut padded_bin_u32 = binary_u8_to_u32(unified_bin);
            pad_bytecode_for_proving(&mut padded_bin_u32);
            let layouts = get_unified_circuit_artifact_for_machine_type::<
                IWithoutByteAccessIsaConfigWithDelegation,
            >(&padded_bin_u32);

            let unrolled_bin =
                recursion_artifact(SECURITY, RecursionLayer::Unrolled, RecursionArtifact::Bin);
            let unrolled_text =
                recursion_artifact(SECURITY, RecursionLayer::Unrolled, RecursionArtifact::Txt);
            let unrolled_setup = compute_setup_for_machine_configuration::<
                IWithoutByteAccessIsaConfigWithDelegation,
            >(
                &padded_bytes(unrolled_bin), &padded_bytes(unrolled_text)
            );

            UnifiedLevelData {
                setup,
                unrolled_setup,
                layouts,
            }
        })
    }

    /// Checks that the chain fields carried by the proof are internally consistent
    /// (mirrors the airbender CLI's `validate_recursion_chain`).
    pub(super) fn validate_recursion_chain(proof: &UnrolledProgramProof) -> Result<(), String> {
        let Some(preimage) = proof.recursion_chain_preimage else {
            return Err("proof is missing recursion_chain_preimage".to_string());
        };
        let Some(hash) = proof.recursion_chain_hash else {
            return Err("proof is missing recursion_chain_hash".to_string());
        };
        let mut hasher = Blake2sBufferingTranscript::new();
        hasher.absorb(&preimage);
        if hasher.finalize().0 != hash {
            return Err("recursion chain hash mismatch".to_string());
        }
        Ok(())
    }

    /// Runs the unified-layer statement verifier and returns the authenticated final register
    /// values.
    pub(super) fn verify(proof: &UnrolledProgramProof) -> Result<[u32; 16], ()> {
        let data = unified_level_data();
        verify_proof_in_unified_layer(proof, &data.setup, &data.layouts, false, SECURITY)
    }
}

fn hash_as_register_values(hash: B256) -> [u32; 8] {
    hash.0
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("Slice with incorrect length")))
        .collect::<Vec<u32>>()
        .try_into()
        .expect("Hash should be exactly 32 bytes long")
}

fn extract_final_register_values(
    input_program_proof: execution_utils_prev::ProgramProof,
) -> [u32; 16] {
    let (metadata, proof_list) =
        execution_utils_prev::ProgramProof::to_metadata_and_proof_list(input_program_proof);

    let oracle_data = execution_utils_prev::generate_oracle_data_from_metadata_and_proof_list(
        &metadata,
        &proof_list,
    );
    tracing::debug!(
        "Oracle data iterator created with {} items",
        oracle_data.len()
    );

    let it = oracle_data.into_iter();

    full_statement_verifier_prev::verifier_common::prover::nd_source_std::set_iterator(it);

    // Assume that program proof has only recursion proofs.
    tracing::debug!("Running continue recursive");
    assert!(metadata.reduced_proof_count > 0);

    let final_register_values = full_statement_verifier_prev::verify_recursion_layer();

    assert!(
        full_statement_verifier_prev::verifier_common::prover::nd_source_std::try_read_word()
            .is_none(),
        "Expected that all words from CSR were consumed"
    );
    final_register_values
}

#[cfg(test)]
mod tests {
    use super::unified_verifier;
    use zksync_os_types::{FriVerificationStrategy, proving_registry};

    fn unified_app_end_params() -> &'static [u32; 8] {
        proving_registry()
            .iter()
            .map(|entry| entry.configuration)
            .find_map(|config| match config.fri.verification {
                FriVerificationStrategy::AirbenderUnifiedLayer {
                    application_end_params,
                } => Some(application_end_params),
                FriVerificationStrategy::AirbenderProgramProof => None,
            })
            .expect("registry must contain a unified-layer proving stack")
    }

    /// Cross-check that the runtime-derived expected recursion chain matches the value computed
    /// offline from the registered app binary and the embedded recursion programs at the pinned
    /// airbender rev (see the registered application-binding parameters).
    #[test]
    #[ignore = "recomputes recursion program setups; slow"]
    fn unified_expected_recursion_chain_matches_offline_computation() {
        assert_eq!(
            unified_verifier::unified_level_data().expected_chain(unified_app_end_params()),
            [
                3908330635, 3818926154, 688684577, 1308736155, 1264132119, 1537631312, 358892107,
                1291547267,
            ],
        );
    }

    /// Smoke test for the unified-layer verifier against a real proof produced by the
    /// airbender CLI (at the rev pinned in the root `Cargo.toml`) with `--target recursion-unified`.
    ///
    /// Run manually:
    ///   UNIFIED_PROOF_ARTIFACT_JSON=/path/to/proof.json \
    ///     cargo test -p zksync_os_server --release unified_layer_verifies_cli_proof -- --ignored
    #[test]
    #[ignore = "needs a locally produced unified-layer proof artifact"]
    fn unified_layer_verifies_cli_proof() {
        let path = std::env::var("UNIFIED_PROOF_ARTIFACT_JSON")
            .expect("set UNIFIED_PROOF_ARTIFACT_JSON to an airbender CLI proof.json");
        let artifact: serde_json::Value =
            serde_json::from_reader(std::fs::File::open(&path).expect("cannot open proof file"))
                .expect("proof file is not valid JSON");
        // The CLI stores `ProofArtifact { proof: UnrolledProgramProof, .. }`.
        let proof: execution_utils::unrolled::UnrolledProgramProof =
            serde_json::from_value(artifact["proof"].clone())
                .expect("artifact has no deserializable `proof` field");

        unified_verifier::validate_recursion_chain(&proof)
            .expect("proof carries an invalid recursion chain");
        let registers = unified_verifier::verify(&proof)
            .expect("unified-layer proof failed cryptographic verification");
        println!("final register values: {registers:?}");

        let expected_chain =
            unified_verifier::unified_level_data().expected_chain(unified_app_end_params());
        assert_eq!(
            registers[8..16],
            expected_chain,
            "proof recursion chain is not rooted in the registered batch program"
        );
    }
}
