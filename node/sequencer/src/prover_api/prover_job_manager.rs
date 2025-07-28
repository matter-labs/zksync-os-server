//! Concurrent in‑memory queue for FRI prover work.
//!
//! * Incoming jobs are received through an async channel (see
//!   [`ProverJobManager::listen_for_batch_jobs`]).
//! * Job is only consumed from the channel once there is a prover available.
//! * Assigned jobs are added to `ProverJobMap` immediately.
//! * Provers request work via [`pick_next_job`]:
//!     * If there is an already assigned job that has timed out, it is reassigned.
//!     * Otherwise, the next job from channel is assigned and inserted into `ProverJobMap`.
//! * When a proof is submitted and verified:
//!     * It is stored in persistent storage as `BatchEnvelope<FriProof>` (FRI cache).
//!     * It is removed from `ProverJobMap` so the map cannot grow without bounds.

use crate::model::batches::{BatchEnvelope, FriProof, ProverInput};
use crate::prover_api::metrics::PROVER_METRICS;
use crate::prover_api::proof_verifier;
use crate::prover_api::prover_job_map::ProverJobMap;
use serde::Serialize;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};

#[derive(Error, Debug)]
pub enum SubmitError {
    #[error("proof did not pass verification")]
    VerificationFailed,
    #[error("batch {0} is not known to the server")]
    UnknownJob(u64),
    #[error("deserialization failed: {0:?}")]
    DeserializationFailed(bincode_v1::Error),
    #[error("internal error: {0}")]
    Other(String),
}

#[derive(Debug, Serialize)]
pub struct JobState {
    pub batch_number: u64,
    pub assigned_seconds_ago: u64,
}

/// Thread-safe queue for FRI prover work.
///
/// Internals use a `DashMap` through `ProverJobMap`, so this type is `Send + Sync`
/// and can be shared behind an `Arc<ProverJobManager>` for concurrent API handlers.
#[derive(Debug)]
pub struct ProverJobManager {
    // == state ==
    jobs: ProverJobMap,
    // == plumbing ==
    // inbound
    batches_for_prove_receiver: Mutex<mpsc::Receiver<BatchEnvelope<ProverInput>>>,
    // outbound
    batches_with_proof_sender: mpsc::Sender<BatchEnvelope<FriProof>>,
}

impl ProverJobManager {
    pub fn new(
        batches_for_prove_receiver: mpsc::Receiver<BatchEnvelope<ProverInput>>,
        batches_with_proof_sender: mpsc::Sender<BatchEnvelope<FriProof>>,

        assignment_timeout: Duration,
    ) -> Self {
        let jobs = ProverJobMap::new(assignment_timeout);
        Self {
            jobs,
            batches_for_prove_receiver: Mutex::new(batches_for_prove_receiver),
            batches_with_proof_sender,
        }
    }

    /// Picks the **smallest** batch number that is either **pending** (from channel)
    /// or whose assignment has **timed‑out** (from the assigned map).
    /// When picking a fresh job from the channel, it is **inserted into ProverJobMap**
    /// immediately so that it can later be timed-out and reassigned if needed.
    pub fn pick_next_job(&self) -> Option<(u64, ProverInput)> {
        // Prefer a timed-out reassignment.
        if let Some(batch_envelope) = self.jobs.pick_timed_out_job() {
            let batch_number = batch_envelope.batch_number();
            let prover_input = batch_envelope.data;
            return Some((batch_number, prover_input));
        }

        // Otherwise, try to receive a fresh job and immediately insert it.
        // Note: alternatively we could wait for mutex unlock and then access it -
        //       but we return early instead and let the caller poll again
        if let Ok(mut rx) = self.batches_for_prove_receiver.try_lock() {
            match rx.try_recv() {
                Ok(env) => {
                    // Insert a clone into the assigned map; return ownership to the caller.
                    let batch_number = env.batch_number();
                    let prover_input = env.data.clone();
                    tracing::info!(
                        batch_number,
                        assigned_jobs_count = self.jobs.len(),
                        "received new job from channel"
                    );
                    self.jobs.insert(env);
                    Some((batch_number, prover_input))
                }
                Err(_) => None,
            }
        } else {
            tracing::warn!(
                "couldn't lock batches_for_prove_receiver - retuning None even though there might be a job available"
            );
            None
        }
    }

    /// Called by the HTTP handler after verifying a proof. On success the entry
    /// is removed so the map cannot grow without bounds.
    ///
    /// Races:
    /// - We do **not** try to suppress timeouts while verifying. If the job is reassigned
    ///   in parallel, a second prover may also submit a proof. The first successful path
    ///   will remove the job; later submitters will observe `UnknownJob`.
    pub async fn submit_proof(
        &self,
        batch_number: u64,
        proof_bytes: Vec<u8>,
        prover_id: &str,
    ) -> Result<(), SubmitError> {
        // Snapshot the assigned job entry (if any).
        let assigned = match self.jobs.get(batch_number) {
            Some(e) => e,
            None => return Err(SubmitError::UnknownJob(batch_number)),
        };

        // Metrics: observe time since the last assignment.
        let prove_time = assigned.assigned_at.elapsed();
        let label: &'static str = Box::leak(prover_id.to_owned().into_boxed_str());
        PROVER_METRICS.prove_time[&label].observe(prove_time);
        PROVER_METRICS.prove_time_per_tx[&label]
            .observe(prove_time / assigned.batch_envelope.batch.tx_count as u32);

        let program_proof = bincode_v1::deserialize(&proof_bytes).map_err(|err| {
            tracing::warn!(batch_number, "failed to deserialize proof: {err}");
            SubmitError::DeserializationFailed(err)
        })?;

        // Verify using metadata from the batch.
        let meta = &assigned.batch_envelope.batch;
        if let Err(err) = proof_verifier::verify_fri_proof(
            meta.previous_stored_batch_info.state_commitment,
            meta.commit_batch_info.clone().into(),
            program_proof,
        ) {
            tracing::warn!(batch_number, "Proof verification failed: {err}");
            return Err(SubmitError::VerificationFailed);
        }

        // Remove the job from the assigned map. If already removed due to a race
        // (another submit won), we treat it as success to keep the API idempotent.
        if self.jobs.remove(batch_number).is_none() {
            tracing::warn!(
                batch_number,
                "Proof persisted; job already removed (racing submit)"
            );
        } else {
            tracing::info!(batch_number, "Proof verified");
        }

        let envelope = BatchEnvelope {
            batch: assigned.batch_envelope.batch,
            data: FriProof::Real(proof_bytes),
            trace: assigned.batch_envelope.trace.with_stage("proof_generated"),
        };
        self.batches_with_proof_sender
            .send(envelope)
            .await
            .map_err(|err| SubmitError::Other(err.to_string()))?;

        Ok(())
    }

    pub fn status(&self) -> Vec<JobState> {
        self.jobs.status()
    }
}
