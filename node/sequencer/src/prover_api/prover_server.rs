use crate::prover_api::prover_job_manager::{ProverJobManager, SubmitError};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;
use tracing::{error, info};

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
    fri_proofs: Vec<String>, // base64‑encoded FRI proofs
}

#[derive(Debug, Serialize, Deserialize)]
struct SnarkProofPayload {
    block_number_from: u64,
    block_number_to: u64,
    proof: String,
}

#[derive(Debug, Deserialize)]
struct ProverQuery {
    id: Option<String>,
}

#[derive(Clone)]
struct AppState {
    job_manager: Arc<ProverJobManager>,
}

/// Public view of job manager state
#[derive(Debug, Serialize)]
pub struct ProverJobManagerState {
    /// List of FRI jobs that are in memory
    pub fri_jobs: Vec<FriJobState>,
    /// (from_block, to_block) of the next SNARK job to be processed
    pub next_snark_job: Option<(u64, u64)>,
    /// estimated number of blocks for which we have FRI proofs (computed and persisted)
    pub fri_proofs_count_estimate: u64,
    /// estimated number of computed and persisted SNARK proofs
    pub snark_proofs_count_estimated: u64,
}

/// Status of a FRI job in memory
#[derive(Debug, Serialize)]
pub enum JobStatusInfo {
    Pending,
    Assigned { seconds_ago: u32 },
}

/// Public view of one FRI job’s state.
#[derive(Debug, Serialize)]
pub struct FriJobState {
    pub block_number: u64,
    pub status: JobStatusInfo,
}

// ───────────── FRI handlers ─────────────

async fn status(State(state): State<AppState>) -> Response {
    let status = state.job_manager.status();
    Json(status).into_response()
}
async fn pick_fri_job(State(state): State<AppState>) -> Response {
    match state.job_manager.pick_next_fri_job() {
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
        .job_manager
        .submit_fri_proof(payload.block_number, proof_bytes, prover_id)
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

async fn get_fri_proof(Path(block): Path<u64>, State(state): State<AppState>) -> Response {
    match state.job_manager.get_fri_proof(block) {
        Some(bytes) => Json(FriProofPayload {
            block_number: block,
            proof: general_purpose::STANDARD.encode(&bytes),
        })
        .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

// ───────────── SNARK handlers ─────────────

async fn pick_snark_job(State(state): State<AppState>) -> Response {
    let Some((from_block, to_block, proofs)) = state.job_manager.get_next_snark_job() else {
        return StatusCode::NO_CONTENT.into_response();
    };

    let fri_proofs_b64: Vec<_> = proofs
        .into_iter()
        .map(|p| general_purpose::STANDARD.encode(&p))
        .collect();

    Json(NextSnarkProverJobPayload {
        block_number_from: from_block,
        block_number_to: to_block,
        fri_proofs: fri_proofs_b64,
    })
    .into_response()
}

async fn submit_snark_proof(
    State(state): State<AppState>,
    Json(payload): Json<SnarkProofPayload>,
) -> Result<Response, (StatusCode, String)> {
    let proof_bytes = general_purpose::STANDARD
        .decode(&payload.proof)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid base64: {e}")))?;

    match state.job_manager.submit_snark_proof(
        payload.block_number_from,
        payload.block_number_to,
        proof_bytes,
    ) {
        Ok(()) => Ok((StatusCode::NO_CONTENT, "proof accepted".to_string()).into_response()),
        Err(e) => {
            error!("internal error: {e}");
            Err((StatusCode::BAD_REQUEST, e.to_string()))
        }
    }
}

// ───────────── Bootstrap ─────────────

pub async fn run(job_manager: Arc<ProverJobManager>, bind_address: String) -> anyhow::Result<()> {
    let app_state = AppState { job_manager };

    let app = Router::new()
        .route("/prover-jobs/status", get(status))
        // FRI routes
        .route("/prover-jobs/FRI/pick", post(pick_fri_job))
        .route("/prover-jobs/FRI/submit", post(submit_fri_proof))
        .route("/prover-jobs/FRI/:block", get(get_fri_proof))
        // SNARK routes
        .route("/prover-jobs/SNARK/pick", post(pick_snark_job))
        .route("/prover-jobs/SNARK/submit", post(submit_snark_proof))
        .with_state(app_state);

    let bind_address: SocketAddr = bind_address.parse()?;
    info!("starting proof data server on {bind_address}");

    let listener = TcpListener::bind(bind_address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
