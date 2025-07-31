use crate::prover_api::proof_storage::ProofStorage;
use crate::prover_api::prover_job_manager::{ProverJobManager, SubmitError};
use axum::extract::Path;
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
use zksync_os_l1_sender::model::{BatchEnvelope, FriProof};
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

#[derive(Debug, Deserialize)]
struct ProverQuery {
    id: Option<String>,
}

// ───────────── Application state ─────────────
#[derive(Clone)]
struct AppState {
    job_manager: Arc<ProverJobManager>,
    proof_storage: ProofStorage,
}

// ───────────── HTTP handlers ─────────────

async fn pick_fri_job(State(state): State<AppState>) -> Response {
    // for real provers, we return the next job immediately -
    // see `FakeProversPool` for fake provers implementation
    match state.job_manager.pick_next_job(Duration::from_secs(0)) {
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
    Query(query): Query<ProverQuery>,
    State(state): State<AppState>,
    Json(payload): Json<ProofPayload>,
) -> Result<Response, (StatusCode, String)> {
    let proof_bytes = general_purpose::STANDARD
        .decode(&payload.proof)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid base64: {e}")))?;

    let prover_id = query.id.as_deref().unwrap_or("unknown_prover");
    match state
        .job_manager
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

async fn get_fri_proof(Path(block): Path<u64>, State(state): State<AppState>) -> Response {
    match state.proof_storage.get(block) {
        Ok(Some(BatchEnvelope {
            data: FriProof::Real(proof_bytes),
            ..
        })) => Json(ProofPayload {
            block_number: block,
            proof: general_purpose::STANDARD.encode(&proof_bytes),
        })
        .into_response(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            error!("error getting proof: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn status(State(state): State<AppState>) -> Response {
    let status = state.job_manager.status();
    Json(status).into_response()
}
pub async fn run(
    job_manager: Arc<ProverJobManager>,
    proof_storage: ProofStorage,
    bind_address: String,
) -> anyhow::Result<()> {
    let app_state = AppState {
        job_manager,
        proof_storage,
    };

    let app = Router::new()
        .route("/prover-jobs/status", get(status))
        .route("/prover-jobs/FRI/pick", post(pick_fri_job))
        .route("/prover-jobs/FRI/submit", post(submit_fri_proof))
        // this method is only used in prover e2e test -
        // it shouldn't be here otherwise. If we want to expose FRI proofs,
        // we need to extract FRI cache to a separate service
        .route("/prover-jobs/FRI/:block", get(get_fri_proof))
        .with_state(app_state);

    let bind_address: SocketAddr = bind_address.parse()?;
    info!("starting proof data server on {bind_address}");

    let listener = TcpListener::bind(bind_address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
