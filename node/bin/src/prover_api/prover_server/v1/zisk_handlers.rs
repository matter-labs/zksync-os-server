//! ZiSK prover HTTP API: per-batch pick/submit for the second proof lane and
//! the aggregation pick/submit that collapses a SNARK range into one proof.
//! The shared v1 router calls [`zisk_routes`]; nothing here touches the
//! Airbender FRI/SNARK endpoints.
//!
//! Transport lives here, beside the Airbender handlers: the wire payloads, the
//! base64 boundary, and the mapping from a manager's typed error to a status
//! code. `zisk_prover_lane` deals in Rust values and `thiserror` enums and
//! knows nothing about HTTP.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use zisk_prover_lane::{
    BatchRange, ZiskAggregationCounts, ZiskAggregationSubmitError, ZiskQueueCounts, ZiskSubmitError,
};

use crate::prover_api::prover_server::{AppState, v1::models::ProverQuery};

/// Response for the ZiSK batch-data pick (and peek) endpoints.
#[derive(Debug, Serialize, Deserialize)]
pub struct ZiskBatchDataPayload {
    pub batch_number: u64,
    pub vk_hash: String,
    /// Base64-encoded bincode-serialized BatchInput for the ZiSK prover.
    pub zisk_data: String,
}

/// Payload for submitting a per-batch ZiSK proof.
///
/// Per-batch PLONK mode: `proof` is the 768-byte wrapped SNARK and
/// `public_values` the 320-byte wire layout. Aggregated mode: `proof` is
/// the raw `vadcop_final` proof stream (~330 KiB, it carries its own
/// publics) and `public_values` must be empty.
#[derive(Debug, Serialize, Deserialize)]
pub struct ZiskProofPayload {
    pub batch_number: u64,
    /// Base64-encoded proof (see above for the per-mode shape).
    pub proof: String,
    /// Base64-encoded ZiSK public values (320 bytes in PLONK mode; empty
    /// in aggregated mode).
    #[serde(default)]
    pub public_values: String,
}

/// One per-batch entry inside a ZiSK aggregation job payload: the raw
/// `vadcop_final` proof stream the server buffered for that batch — the
/// exact input the aggregator guest verifies.
#[derive(Debug, Serialize, Deserialize)]
pub struct ZiskAggregationBatchProof {
    pub batch_number: u64,
    /// Base64-encoded per-batch `vadcop_final` proof stream (~330 KiB).
    pub proof: String,
}

/// Response for the ZiSK aggregation pick endpoint: a contiguous range of
/// buffered per-batch `vadcop_final` streams, in batch order.
#[derive(Debug, Serialize, Deserialize)]
pub struct ZiskAggregationJobPayload {
    pub from_batch_number: u64,
    pub to_batch_number: u64,
    pub proofs: Vec<ZiskAggregationBatchProof>,
}

/// Payload for submitting an aggregated ZiSK range proof.
#[derive(Debug, Serialize, Deserialize)]
pub struct ZiskAggregationProofPayload {
    pub from_batch_number: u64,
    pub to_batch_number: u64,
    /// Base64-encoded aggregated ZiSK SNARK proof (768 bytes).
    pub proof: String,
    /// Base64-encoded aggregated public values (320 bytes; the aggregator
    /// guest's binding digest sits at bytes [32..64]).
    pub public_values: String,
}

/// Response for the ZiSK lane status endpoint: what each stage of the lane
/// holds right now. `per_batch.proofs_completed` and
/// `aggregation.range_proofs_completed` count ACCEPTED submissions that
/// survived validation and are still parked, so a caller can tell an accepted
/// proof from one the server took and dropped.
#[derive(Debug, Serialize, Deserialize)]
pub struct ZiskLaneStatusPayload {
    pub per_batch: ZiskQueueCounts,
    pub aggregation: ZiskAggregationCounts,
}

/// The ZiSK routes, mounted under the same `/prover-jobs/v1` prefix as the
/// Airbender routes so both prover fleets share one endpoint.
pub(in crate::prover_api::prover_server) fn zisk_routes() -> Router<AppState> {
    Router::new()
        // ZiSK SNARK prover routes
        .route("/ZiSK/pick", post(pick_zisk_job))
        .route("/ZiSK/submit", post(submit_zisk_proof))
        // ZiSK aggregation routes: range collapse for L1
        .route("/ZiSK-AGG/pick", post(pick_zisk_aggregation_job))
        .route("/ZiSK-AGG/submit", post(submit_zisk_aggregation_proof))
        // observability routes
        .route("/ZiSK/status", get(zisk_lane_status))
        .route("/ZiSK/{batch_number}/peek", get(peek_zisk_data))
}

fn decode_base64(field: &'static str, value: &str) -> Result<Vec<u8>, (StatusCode, String)> {
    general_purpose::STANDARD.decode(value).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid {field} base64: {e}"),
        )
    })
}

/// Preserve the distinction between malformed work, a superseded lease, and a
/// completion path that is unavailable because the pipeline is shutting down.
fn per_batch_status(error: &ZiskSubmitError) -> StatusCode {
    match error {
        ZiskSubmitError::UnknownJob(_) => StatusCode::NOT_FOUND,
        ZiskSubmitError::Superseded(_) => StatusCode::CONFLICT,
        ZiskSubmitError::CompletionUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    }
}

fn aggregation_status(error: &ZiskAggregationSubmitError) -> StatusCode {
    match error {
        ZiskAggregationSubmitError::UnknownRange { .. } => StatusCode::NOT_FOUND,
        ZiskAggregationSubmitError::Superseded { .. } => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    }
}

/// Report what the ZiSK lane holds: per-batch queues and aggregation-stage
/// queues, including how many accepted proofs are parked for the rendezvous.
async fn zisk_lane_status(State(state): State<AppState>) -> Response {
    let (Some(zisk_job_manager), Some(zisk_aggregation_job_manager)) =
        (&state.zisk_job_manager, &state.zisk_aggregation_job_manager)
    else {
        return (StatusCode::SERVICE_UNAVAILABLE, "ZiSK proving not enabled").into_response();
    };
    Json(ZiskLaneStatusPayload {
        per_batch: zisk_job_manager.queue_counts().await,
        aggregation: zisk_aggregation_job_manager.queue_counts().await,
    })
    .into_response()
}

/// Peek ZiSK batch data for a given batch number.
/// Returns the bincode-serialized BatchInput for the ZiSK prover.
///
/// The VK hash comes from the Airbender FRI job map — the batch's presence
/// there is the 204-vs-404 signal — and the bytes from the ZiSK job manager.
async fn peek_zisk_data(Path(batch_number): Path<u64>, State(state): State<AppState>) -> Response {
    let Some(vk_hash) = state.fri_job_manager.peek_fri_vk_hash(batch_number).await else {
        // Not in the FRI job map (yet, or already consumed).
        return StatusCode::NO_CONTENT.into_response();
    };
    let zisk_bytes = match &state.zisk_job_manager {
        Some(zisk_job_manager) => zisk_job_manager.peek_input(batch_number).await,
        None => None,
    };
    match zisk_bytes {
        Some(zisk_bytes) => Json(ZiskBatchDataPayload {
            batch_number,
            vk_hash,
            zisk_data: general_purpose::STANDARD.encode(&zisk_bytes),
        })
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            format!("Batch {batch_number} has no ZiSK data (second_proof_system not enabled?)"),
        )
            .into_response(),
    }
}

/// Pick the next ZiSK SNARK job. Assigns a batch to the requesting prover.
/// Mirrors `/FRI/pick` in semantics: assignment with timeout-based reassignment.
async fn pick_zisk_job(
    Query(query): Query<ProverQuery>,
    State(state): State<AppState>,
) -> Response {
    let Some(ref zisk_job_manager) = state.zisk_job_manager else {
        return (StatusCode::SERVICE_UNAVAILABLE, "ZiSK proving not enabled").into_response();
    };
    match zisk_job_manager.pick_next_job(&query.id).await {
        Some(job) => Json(ZiskBatchDataPayload {
            batch_number: job.batch_number,
            vk_hash: job.vk_hash,
            zisk_data: general_purpose::STANDARD.encode(&job.zisk_data),
        })
        .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Submit a per-batch ZiSK proof: the PLONK-wrapped SNARK (per-batch mode,
/// composed with the Airbender SNARK into a MultiProof) or the raw
/// `vadcop_final` stream (aggregated mode, buffered as aggregation input).
async fn submit_zisk_proof(
    Query(query): Query<ProverQuery>,
    State(state): State<AppState>,
    Json(payload): Json<ZiskProofPayload>,
) -> Result<Response, (StatusCode, String)> {
    let Some(ref zisk_job_manager) = state.zisk_job_manager else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "ZiSK proving not enabled".into(),
        ));
    };
    let proof = decode_base64("proof", &payload.proof)?;
    let public_values = decode_base64("public_values", &payload.public_values)?;

    zisk_job_manager
        .submit_proof(payload.batch_number, proof, public_values, &query.id)
        .await
        .map_err(|e| (per_batch_status(&e), format!("{e}")))?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Pick the next ZiSK AGGREGATION job: the buffered per-batch
/// `vadcop_final` streams of one Airbender SNARK range, to collapse into
/// one aggregator-guest proof. Mirrors `/SNARK/pick` semantics (range job,
/// timeout-based reassignment).
async fn pick_zisk_aggregation_job(
    Query(query): Query<ProverQuery>,
    State(state): State<AppState>,
) -> Response {
    let Some(ref zisk_aggregation_job_manager) = state.zisk_aggregation_job_manager else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "ZiSK aggregation not enabled",
        )
            .into_response();
    };
    match zisk_aggregation_job_manager.pick_next_job(&query.id).await {
        Some(job) => Json(ZiskAggregationJobPayload {
            from_batch_number: job.from_batch,
            to_batch_number: job.to_batch,
            proofs: job
                .streams
                .into_iter()
                .map(|(batch_number, stream)| ZiskAggregationBatchProof {
                    batch_number,
                    proof: general_purpose::STANDARD.encode(&stream),
                })
                .collect(),
        })
        .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Submit an aggregated ZiSK range proof. Validated (aggregator program-VK
/// tripwire + the binding digest recomputed from the buffered per-batch
/// streams) and parked until the Airbender SNARK of the same range
/// composes the MultiProof.
async fn submit_zisk_aggregation_proof(
    Query(query): Query<ProverQuery>,
    State(state): State<AppState>,
    Json(payload): Json<ZiskAggregationProofPayload>,
) -> Result<Response, (StatusCode, String)> {
    let Some(ref zisk_aggregation_job_manager) = state.zisk_aggregation_job_manager else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "ZiSK aggregation not enabled".into(),
        ));
    };
    // The bounds arrive on the wire, so this is where they are checked.
    let range = BatchRange::new(payload.from_batch_number, payload.to_batch_number)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
    let proof = decode_base64("proof", &payload.proof)?;
    let public_values = decode_base64("public_values", &payload.public_values)?;

    zisk_aggregation_job_manager
        .submit_proof(range, proof, public_values, &query.id)
        .await
        .map_err(|e| (aggregation_status(&e), format!("{e}")))?;

    Ok(StatusCode::NO_CONTENT.into_response())
}
