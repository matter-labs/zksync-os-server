use crate::prover_api::fri_job_manager::{FriJobManager, SubmitError};
use crate::prover_api::proof_storage::ProofStorage;
use crate::prover_api::snark_job_manager::SnarkJobManager;
use axum::extract::DefaultBodyLimit;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;
use tracing::{error, info};
use zksync_os_l1_sender::batcher_model::FriProof;
// ───────────── JSON payloads ─────────────

#[derive(Debug, Serialize, Deserialize)]
struct NextFriProverJobPayload {
    block_number: u64,
    prover_input: String, // base64‑encoded little‑endian u32 array
}

#[derive(Debug, Serialize, Deserialize)]
struct FriProofPayload {
    block_number: u64,
    proof: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct NextSnarkProverJobPayload {
    block_number_from: u64,
    block_number_to: u64,
    fri_proofs: Vec<String>, // base64‑encoded FRI proofs (little‑endian u32 array)
}

#[derive(Debug, Serialize, Deserialize)]
struct SnarkProofPayload {
    block_number_from: u64,
    block_number_to: u64,
    proof: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AvailableProofsPayload {
    block_number: u64,
    available_proofs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BatchDataPayload {
    batch_number: u64,
    prover_input: String,
}

#[derive(Debug, Deserialize)]
struct ProverQuery {
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BatchDataQuery {
    batch_number: u64,
}

// ───────────── Application state ─────────────
#[derive(Clone)]
struct AppState {
    fri_job_manager: Arc<FriJobManager>,
    snark_job_manager: Arc<SnarkJobManager>,
}

// ───────────── HTTP handlers ─────────────

async fn pick_fri_job(State(state): State<AppState>) -> Response {
    // for real provers, we return the next job immediately -
    // see `FakeProversPool` for fake provers implementation
    match state.fri_job_manager.pick_next_job(Duration::from_secs(0)) {
        Some((block, input)) => {
            let bytes: Vec<u8> = input.iter().flat_map(|v| v.to_le_bytes()).collect();
            Json(NextFriProverJobPayload {
                block_number: block,
                prover_input: general_purpose::STANDARD.encode(&bytes),
            })
            .into_response()
        }
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn submit_fri_proof(
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
        .submit_proof(payload.block_number, proof_bytes, prover_id)
        .await
    {
        Ok(()) => Ok((StatusCode::NO_CONTENT, "proof accepted".to_string()).into_response()),
        Err(SubmitError::VerificationFailed) => Err((
            StatusCode::BAD_REQUEST,
            "proof verification failed".to_string(),
        )),
        Err(SubmitError::UnknownJob(_)) => Err((StatusCode::NOT_FOUND, "unknown block".into())),
        Err(SubmitError::DeserializationFailed(err)) => {
            Err((StatusCode::BAD_REQUEST, err.to_string()))
        }
        Err(SubmitError::Other(e)) => {
            error!("internal error: {e}");
            Err((StatusCode::INTERNAL_SERVER_ERROR, e))
        }
    }
}

async fn pick_snark_job(State(state): State<AppState>) -> Response {
    match state.snark_job_manager.pick_real_job().await {
        Ok(Some(batches)) => {
            // Expect non-empty and all real FRI proofs
            let from = batches.first().unwrap().0;
            let to = batches.last().unwrap().0;

            let fri_proofs = batches
                .into_iter()
                .filter_map(|(batch_number, proof)| match proof {
                    FriProof::Real(real) => Some(general_purpose::STANDARD.encode(real.proof())),
                    FriProof::Fake => {
                        // Should never happen; defensive guard
                        error!(
                            "SNARK pick returned fake FRI at batch {} (range {}-{})",
                            batch_number, from, to
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
            error!("error picking SNARK job: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn submit_snark_proof(
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

async fn peek_batch_data(
    Query(query): Query<BatchDataQuery>,
    State(state): State<AppState>,
) -> Response {
    match state.fri_job_manager.peek_batch_data(query.batch_number) {
        Some(prover_input) => {
            let bytes: Vec<u8> = prover_input.iter().flat_map(|v| v.to_le_bytes()).collect();
            Json(BatchDataPayload {
                batch_number: query.batch_number,
                prover_input: general_purpose::STANDARD.encode(&bytes),
            })
            .into_response()
        }
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn status(State(state): State<AppState>) -> Response {
    let status = state.fri_job_manager.status();
    Json(status).into_response()
}
pub async fn run(
    fri_job_manager: Arc<FriJobManager>,
    snark_job_manager: Arc<SnarkJobManager>,
    _proof_storage: ProofStorage,
    bind_address: String,
) -> anyhow::Result<()> {
    let app_state = AppState {
        fri_job_manager,
        snark_job_manager,
    };

    let app = Router::new()
        .route("/prover-jobs/status", get(status))
        .route("/prover-jobs/FRI/peek", post(peek_batch_data))
        .route("/prover-jobs/FRI/pick", post(pick_fri_job))
        .route("/prover-jobs/FRI/submit", post(submit_fri_proof))
        .route("/prover-jobs/SNARK/pick", post(pick_snark_job))
        .route("/prover-jobs/SNARK/submit", post(submit_snark_proof))
        .with_state(app_state)
        // Set the request body limit to 10MiB
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024));

    let bind_address: SocketAddr = bind_address.parse()?;
    info!("starting proof data server on {bind_address}");

    let listener = TcpListener::bind(bind_address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
