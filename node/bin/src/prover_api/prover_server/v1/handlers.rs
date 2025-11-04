use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose};
use http::StatusCode;
use zksync_os_l1_sender::batcher_model::FriProof;
use zksync_os_multivm::ExecutionVersion;

use crate::prover_api::{
    fri_job_manager::SubmitError,
    prover_server::{
        AppState,
        v1::models::{
            BatchDataPayload, FailedProofResponse, FriProofPayload, NextSnarkProverJobPayload,
            PickJobPayload, ProverQuery, SnarkProofPayload,
        },
    },
};

pub(super) async fn pick_fri_job(State(state): State<AppState>) -> Response {
    // for real provers, we return the next job immediately -
    // see `FakeProversPool` for fake provers implementation
    match state.fri_job_manager.pick_next_job(Duration::from_secs(0)) {
        Some((block, vk_hash, input)) => {
            let bytes: Vec<u8> = input.iter().flat_map(|v| v.to_le_bytes()).collect();
            Json(BatchDataPayload {
                block_number: block,
                vk_hash: vk_hash.to_string(),
                prover_input: general_purpose::STANDARD.encode(&bytes),
            })
            .into_response()
        }
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

pub(super) async fn submit_fri_proof(
    Query(query): Query<ProverQuery>,
    State(state): State<AppState>,
    Json(payload): Json<FriProofPayload>,
) -> Result<Response, (StatusCode, String)> {
    let proof_bytes = general_purpose::STANDARD
        .decode(&payload.proof)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid base64: {e}")))?;

    let prover_id = query.id;
    let execution_version = ExecutionVersion::try_from_vk_hash(&payload.vk_hash).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("no Execution Version matches the provided verification key: {e}"),
        )
    })?;
    match state
        .fri_job_manager
        .submit_proof(payload.block_number, proof_bytes.into(), execution_version, &prover_id)
        .await
    {
        Ok(()) => Ok((StatusCode::NO_CONTENT, "proof accepted".to_string()).into_response()),
        Err(SubmitError::VerificationKeyHashMismatch(server_vk, prover_vk)) => Err((
            StatusCode::BAD_REQUEST,
            format!(
                "verification key hash mismatch: server has {server_vk}, prover used {prover_vk}"
            )
            .to_string(),
        )),
        Err(SubmitError::FriProofVerificationError {
            expected_hash_u32s,
            proof_final_register_values,
        }) => Err((
            StatusCode::BAD_REQUEST,
            format!(
                "FRI proof verification failed. Expected: {expected_hash_u32s:?}, Got: {proof_final_register_values:?}"
            )
            .to_string(),
        )),
        Err(SubmitError::UnknownJob(_)) => Err((StatusCode::NOT_FOUND, "unknown block".into())),
        Err(SubmitError::DeserializationFailed(err)) => {
            Err((StatusCode::BAD_REQUEST, err.to_string()))
        }
        Err(SubmitError::Other(e)) => {
            tracing::error!("internal error: {e}");
            Err((StatusCode::INTERNAL_SERVER_ERROR, e))
        }
    }
}

pub(super) async fn pick_snark_job(State(state): State<AppState>) -> Response {
    match state.snark_job_manager.pick_real_job().await {
        Ok(Some(batches)) => {
            // Expect non-empty and all real FRI proofs
            let from = batches.first().unwrap().0;
            let to = batches.last().unwrap().0;
            let vk_hash = batches.first().unwrap().1.to_string();

            let fri_proofs = batches
                .into_iter()
                .filter_map(|(batch_number, _, proof)| match proof {
                    FriProof::Real(real) => Some(general_purpose::STANDARD.encode(real.proof())),
                    FriProof::Fake => {
                        // Should never happen; defensive guard
                        tracing::error!(
                            "SNARK pick returned fake FRI at batch {} (range {}-{})",
                            batch_number,
                            from,
                            to
                        );
                        None
                    }
                })
                .collect();

            Json(NextSnarkProverJobPayload {
                block_number_from: from,
                block_number_to: to,
                vk_hash,
                fri_proofs,
            })
            .into_response()
        }
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("error picking SNARK job: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(super) async fn submit_snark_proof(
    Query(_query): Query<ProverQuery>,
    State(state): State<AppState>,
    Json(payload): Json<SnarkProofPayload>,
) -> Result<Response, (StatusCode, String)> {
    let proof_bytes = general_purpose::STANDARD
        .decode(&payload.proof)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid base64: {e}")))?;
    let execution_version = ExecutionVersion::try_from_vk_hash(&payload.vk_hash).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("no Execution Version matches the provided verification key: {e}"),
        )
    })?;
    match state
        .snark_job_manager
        .submit_proof(
            payload.block_number_from,
            payload.block_number_to,
            execution_version,
            proof_bytes,
        )
        .await
    {
        Ok(()) => Ok((StatusCode::NO_CONTENT, "proof accepted".to_string()).into_response()),
        Err(err) => Err((
            StatusCode::BAD_REQUEST,
            format!("proof rejected: {err}").to_string(),
        )),
    }
}

pub(super) async fn peek_fri_job(
    Path(batch_number): Path<u64>,
    State(state): State<AppState>,
) -> Response {
    match state.fri_job_manager.peek_batch_data(batch_number) {
        Some((vk_hash, prover_input)) => {
            let bytes: Vec<u8> = prover_input.iter().flat_map(|v| v.to_le_bytes()).collect();
            Json(BatchDataPayload {
                block_number: batch_number,
                vk_hash: vk_hash.to_string(),
                prover_input: general_purpose::STANDARD.encode(&bytes),
            })
            .into_response()
        }
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

pub(super) async fn peek_snark_job(
    Path((from_batch_number, to_batch_number)): Path<(u64, u64)>,
    State(state): State<AppState>,
) -> Response {
    if from_batch_number > to_batch_number {
        return (
            StatusCode::BAD_REQUEST,
            format!("Invalid range: from_batch_number ({from_batch_number}) must be <= to_batch_number ({to_batch_number})")
        ).into_response();
    }

    let mut fri_proofs = vec![];
    let mut vk_hash = String::new();
    for batch_number in from_batch_number..=to_batch_number {
        match state.proof_storage.get_batch_with_proof(batch_number).await {
            Ok(Some(env)) => {
                vk_hash = match env.verification_key_hash() {
                    Ok(vk) => {
                        let new_vk_hash = vk.to_string();
                        if vk_hash != new_vk_hash {
                            tracing::warn!(
                                "Mismatched VK hashes in requested range: previous block had {}, current block {} has {}",
                                vk_hash,
                                batch_number,
                                new_vk_hash,
                            );
                        }
                        vk.to_string()
                    }
                    Err(e) => {
                        tracing::warn!(
                            "There's no VK available for proof at batch {} - error whilst getting VK hash: {e:?}",
                            batch_number
                        );
                        String::new()
                    }
                };
                match env.data {
                    FriProof::Real(real) => {
                        fri_proofs.push(general_purpose::STANDARD.encode(real.proof()))
                    }
                    FriProof::Fake => {
                        tracing::info!(
                            "Requested FRI proof for batch {} is fake (range {}-{})",
                            batch_number,
                            from_batch_number,
                            to_batch_number
                        );
                        return (
                            StatusCode::BAD_REQUEST,
                            format!("FRI proof for batch {batch_number} is fake"),
                        )
                            .into_response();
                    }
                };
            }
            Ok(None) => {
                tracing::info!(
                    "No FRI proof found for batch {batch_number} (range {}-{})",
                    from_batch_number,
                    to_batch_number
                );
                return (
                    StatusCode::NOT_FOUND,
                    format!("No FRI proof found for batch {batch_number}"),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::info!("Error retrieving FRI proof for batch {batch_number}: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Error retrieving proof: {e}"),
                )
                    .into_response();
            }
        }
    }
    Json(NextSnarkProverJobPayload {
        block_number_from: from_batch_number,
        block_number_to: to_batch_number,
        vk_hash,
        fri_proofs,
    })
    .into_response()
}

pub(super) async fn status(State(state): State<AppState>) -> Response {
    let status = state.fri_job_manager.status();
    Json(status).into_response()
}

/// Get detailed information about a failed FRI proof for debugging.
/// Returns the most recent failed proof for the given batch number.
pub(super) async fn get_failed_fri_proof(
    Path(batch_number): Path<u64>,
    State(state): State<AppState>,
) -> Response {
    match state.proof_storage.get_failed_proof(batch_number).await {
        Ok(Some(failed_proof)) => {
            let response = FailedProofResponse {
                batch_number: failed_proof.batch_number,
                last_block_timestamp: failed_proof.last_block_timestamp,
                expected_hash_u32s: failed_proof.expected_hash_u32s,
                proof_final_register_values: failed_proof.proof_final_register_values,
                vk_hash: failed_proof.vk_hash.unwrap_or_default(),
                proof: general_purpose::STANDARD.encode(failed_proof.proof_bytes),
            };

            Json(response).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            format!("No failed proof found for batch {batch_number}"),
        )
            .into_response(),
        Err(e) => {
            tracing::info!("Error retrieving failed proof for batch {batch_number}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error retrieving failed proof: {e}"),
            )
                .into_response()
        }
    }
}
