use crate::prover_api::prover_job_manager::{ProverJobManager, SubmitError};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;
use tracing::{error, info};
// ───────────── JSON payloads ─────────────

#[derive(Debug, Serialize, Deserialize)]
struct NextProverJobPayload {
    block_number: u64,
    prover_input: String, // base64‑encoded little‑endian u32 array
}

#[derive(Debug, Serialize, Deserialize)]
struct ProofPayload {
    block_number: u64,
    proof: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AvailableProofsPayload {
    block_number: u64,
    available_proofs: Vec<String>,
}

#[derive(Clone)]
struct AppState {
    job_manager: Arc<ProverJobManager>,
}

// ───────────── HTTP handlers ─────────────

async fn pick_fri_job(State(state): State<AppState>) -> Response {
    match state.job_manager.pick_next_job("unknown_prover") {
        Some((block, input)) => {
            let bytes: Vec<u8> = input.iter().flat_map(|v| v.to_le_bytes()).collect();
            Json(NextProverJobPayload {
                block_number: block,
                prover_input: general_purpose::STANDARD.encode(&bytes),
            })
            .into_response()
        }
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn submit_fri_proof(
    State(state): State<AppState>,
    Json(payload): Json<ProofPayload>,
) -> Result<Response, (StatusCode, String)> {
    let proof_bytes = general_purpose::STANDARD
        .decode(&payload.proof)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid base64: {e}")))?;

    match state
        .job_manager
        .submit_proof(payload.block_number, proof_bytes)
    {
        Ok(()) => Ok((StatusCode::NO_CONTENT, "proof accepted".to_string()).into_response()),
        Err(SubmitError::VerificationFailed) => Err((
            StatusCode::BAD_REQUEST,
            "proof verification failed".to_string(),
        )),
        Err(SubmitError::UnknownJob(_)) => Err((StatusCode::NOT_FOUND, "unknown block".into())),
        Err(SubmitError::Other(e)) => {
            error!("internal error: {e}");
            Err((StatusCode::INTERNAL_SERVER_ERROR, e))
        }
    }
}

async fn list_available_proofs(State(state): State<AppState>) -> Response {
    let payload: Vec<_> = state
        .job_manager
        .available_proofs()
        .into_iter()
        .map(|(block_number, available_proofs)| AvailableProofsPayload {
            block_number,
            available_proofs,
        })
        .collect();
    Json(payload).into_response()
}

async fn get_fri_proof(Path(block): Path<u64>, State(state): State<AppState>) -> Response {
    match state.job_manager.get_fri_proof(block) {
        Some(bytes) => Json(ProofPayload {
            block_number: block,
            proof: general_purpose::STANDARD.encode(&bytes),
        })
        .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn status(State(state): State<AppState>) -> Response {
    let status = state.job_manager.status();
    Json(status).into_response()
}
pub async fn run(job_manager: Arc<ProverJobManager>, bind_address: String) -> anyhow::Result<()> {
    let app_state = AppState { job_manager };

    let app = Router::new()
        .route("/prover-jobs/status", get(status))
        .route("/prover-jobs/FRI/pick", post(pick_fri_job))
        .route("/prover-jobs/FRI/submit", post(submit_fri_proof))
        .route("/prover-jobs/available", get(list_available_proofs))
        .route("/prover-jobs/FRI/:block", get(get_fri_proof))
        .with_state(app_state);

    let bind_address: SocketAddr = bind_address.parse()?;
    info!("starting proof data server on {bind_address}");

    let listener = TcpListener::bind(bind_address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
