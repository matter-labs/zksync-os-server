//! ZiSK prover HTTP API: per-batch pick/submit for the second proof lane and
//! the aggregation pick/submit that collapses a SNARK range into one proof.
//! The shared v1 router calls [`zisk_routes`]; nothing here touches the
//! Airbender FRI/SNARK endpoints.
//!
//! Only the route registration and the `AppState` wiring live here. The handler
//! bodies (payload shapes, base64 decode/encode, manager calls, error → status
//! mapping) live in `zisk_prover_lane::handlers`; each registered handler
//! unwraps the relevant job manager from `AppState` (a `None` means the feature
//! is off → 503) and delegates. The `peek` route stays here because it reads
//! the Airbender FRI job manager, but reuses the crate's response payload.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose};
use http::StatusCode;
use zisk_prover_lane::handlers::{
    ZiskAggregationProofPayload, ZiskBatchDataPayload, ZiskProofPayload,
    pick_zisk_aggregation_job as pick_zisk_aggregation_job_body,
    pick_zisk_job as pick_zisk_job_body,
    submit_zisk_aggregation_proof as submit_zisk_aggregation_proof_body,
    submit_zisk_proof as submit_zisk_proof_body, zisk_lane_status as zisk_lane_status_body,
};

use crate::prover_api::prover_server::{AppState, v1::models::ProverQuery};

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

/// Report what the ZiSK lane holds: per-batch queues and aggregation-stage
/// queues, including how many accepted proofs are parked for the rendezvous.
async fn zisk_lane_status(State(state): State<AppState>) -> Response {
    let (Some(zisk_job_manager), Some(zisk_aggregation_job_manager)) =
        (&state.zisk_job_manager, &state.zisk_aggregation_job_manager)
    else {
        return (StatusCode::SERVICE_UNAVAILABLE, "ZiSK proving not enabled").into_response();
    };
    zisk_lane_status_body(zisk_job_manager, zisk_aggregation_job_manager).await
}

/// Peek ZiSK batch data for a given batch number.
/// Returns the bincode-serialized BatchInput for the ZiSK prover.
///
/// The vk hash comes from the Airbender FRI job map (the batch's presence there
/// is the 204-vs-404 signal) and the bytes from the ZiSK job manager, which
/// holds the sealed input while the job is active or parked; the FRI lane
/// itself no longer holds ZiSK state.
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
    pick_zisk_job_body(zisk_job_manager, &query.id).await
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
    submit_zisk_proof_body(zisk_job_manager, payload, &query.id).await
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
    pick_zisk_aggregation_job_body(zisk_aggregation_job_manager, &query.id).await
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
    submit_zisk_aggregation_proof_body(zisk_aggregation_job_manager, payload, &query.id).await
}
