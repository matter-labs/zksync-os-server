use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose};
use http::StatusCode;
use zksync_os_l1_sender::batcher_model::FriProof;

use crate::prover_api::{
    fri_job_manager::{JobStateLegacy, SubmitError},
    prover_server::{
        AppState,
        legacy::models::{
            BatchDataPayload, FailedProofResponse, FriProofPayload, NextSnarkProverJobPayload,
            ProverQuery, SnarkProofPayload,
        },
    },
};

pub(super) async fn pick_fri_job(State(state): State<AppState>) -> Response {
    // for real provers, we return the next job immediately -
    // see `FakeProversPool` for fake provers implementation
    match state.fri_job_manager.pick_next_job(Duration::from_secs(0)) {
        Some((fri_job, input)) => {
            let bytes: Vec<u8> = input.iter().flat_map(|v| v.to_le_bytes()).collect();
            Json(BatchDataPayload {
                block_number: fri_job.batch_number,
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

    let prover_id = query.id.as_deref().unwrap_or("unknown_prover");
    match state
        .fri_job_manager
        .submit_proof(payload.block_number, proof_bytes.into(), None, prover_id)
        .await
    {
        Ok(()) => Ok((StatusCode::NO_CONTENT, "proof accepted".to_string()).into_response()),
        Err(SubmitError::ExecutionVersionMismatch(_, _)) =>
            panic!("Should never happen, as provers don't provide execution_version"),
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
            let from = batches.first().unwrap().0.batch_number;
            let to = batches.last().unwrap().0.batch_number;

            let fri_proofs = batches
                .into_iter()
                .filter_map(|(fri_job, proof)| match proof {
                    FriProof::Real(real) => Some(general_purpose::STANDARD.encode(real.proof())),
                    FriProof::Fake => {
                        // Should never happen; defensive guard
                        tracing::error!(
                            "SNARK pick returned fake FRI at batch {} (range {}-{})",
                            fri_job.batch_number,
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

    match state
        .snark_job_manager
        .submit_proof(
            payload.block_number_from,
            payload.block_number_to,
            None,
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

pub(super) async fn peek_batch_data(
    Path(batch_number): Path<u64>,
    State(state): State<AppState>,
) -> Response {
    match state.fri_job_manager.peek_batch_data(batch_number) {
        Some((_, prover_input)) => {
            let bytes: Vec<u8> = prover_input.iter().flat_map(|v| v.to_le_bytes()).collect();
            Json(BatchDataPayload {
                block_number: batch_number,
                prover_input: general_purpose::STANDARD.encode(&bytes),
            })
            .into_response()
        }
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

pub(super) async fn peek_fri_proofs(
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
    for batch_number in from_batch_number..=to_batch_number {
        match state.proof_storage.get_batch_with_proof(batch_number).await {
            Ok(Some(env)) => {
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
        fri_proofs,
    })
    .into_response()
}

pub(super) async fn status(State(state): State<AppState>) -> Response {
    let status: Vec<JobStateLegacy> = state
        .fri_job_manager
        .status()
        .into_iter()
        .map(|state| state.into())
        .collect();
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
