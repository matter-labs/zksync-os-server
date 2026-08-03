//! HTTP handler bodies for the ZiSK prover API: per-batch pick/submit for the
//! second proof lane and the aggregation pick/submit that collapses a SNARK
//! range into one proof.
//!
//! These are the request/response payloads and the logic (base64 decode/encode,
//! manager calls, error → status mapping). The axum route registration and the
//! `AppState` wiring stay in `node/bin`; each registered handler extracts the
//! relevant job manager and calls into here. The `peek` route stays in
//! `node/bin` (it reads the Airbender FRI job manager), but reuses
//! [`ZiskBatchDataPayload`].

use axum::{
    Json,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose};
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::aggregation_job_manager::{
    ZiskAggregationCounts, ZiskAggregationJobManager, ZiskAggregationSubmitError,
};
use crate::job_manager::{ZiskJobManager, ZiskQueueCounts};

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

/// Report what the ZiSK lane holds: per-batch queues and aggregation-stage
/// queues. Mirrors `/status/` on the Airbender side.
pub async fn zisk_lane_status(
    zisk_job_manager: &ZiskJobManager,
    zisk_aggregation_job_manager: &ZiskAggregationJobManager,
) -> Response {
    Json(ZiskLaneStatusPayload {
        per_batch: zisk_job_manager.queue_counts().await,
        aggregation: zisk_aggregation_job_manager.queue_counts().await,
    })
    .into_response()
}

/// Pick the next ZiSK SNARK job. Assigns a batch to the requesting prover.
/// Mirrors `/FRI/pick` in semantics: assignment with timeout-based reassignment.
pub async fn pick_zisk_job(zisk_job_manager: &ZiskJobManager, prover_id: &str) -> Response {
    match zisk_job_manager.pick_next_job(prover_id).await {
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
pub async fn submit_zisk_proof(
    zisk_job_manager: &ZiskJobManager,
    payload: ZiskProofPayload,
    prover_id: &str,
) -> Result<Response, (StatusCode, String)> {
    let proof = general_purpose::STANDARD
        .decode(&payload.proof)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid proof base64: {e}"),
            )
        })?;
    let public_values = general_purpose::STANDARD
        .decode(&payload.public_values)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid public_values base64: {e}"),
            )
        })?;

    zisk_job_manager
        .submit_proof(payload.batch_number, proof, public_values, prover_id)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Pick the next ZiSK AGGREGATION job: the buffered per-batch
/// `vadcop_final` streams of one Airbender SNARK range, to collapse into
/// one aggregator-guest proof. Mirrors `/SNARK/pick` semantics (range job,
/// timeout-based reassignment).
pub async fn pick_zisk_aggregation_job(
    zisk_aggregation_job_manager: &ZiskAggregationJobManager,
    prover_id: &str,
) -> Response {
    match zisk_aggregation_job_manager.pick_next_job(prover_id).await {
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
pub async fn submit_zisk_aggregation_proof(
    zisk_aggregation_job_manager: &ZiskAggregationJobManager,
    payload: ZiskAggregationProofPayload,
    prover_id: &str,
) -> Result<Response, (StatusCode, String)> {
    let proof = general_purpose::STANDARD
        .decode(&payload.proof)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid proof base64: {e}"),
            )
        })?;
    let public_values = general_purpose::STANDARD
        .decode(&payload.public_values)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid public_values base64: {e}"),
            )
        })?;

    match zisk_aggregation_job_manager
        .submit_proof(
            payload.from_batch_number,
            payload.to_batch_number,
            proof,
            public_values,
            prover_id,
        )
        .await
    {
        Ok(()) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(err @ ZiskAggregationSubmitError::UnknownRange { .. }) => {
            Err((StatusCode::NOT_FOUND, format!("{err}")))
        }
        Err(err) => Err((StatusCode::BAD_REQUEST, format!("{err}"))),
    }
}
