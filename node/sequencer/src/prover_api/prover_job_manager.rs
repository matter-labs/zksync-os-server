//! Concurrent in‑memory queue for FRI prover work.
//!
//! * Incoming jobs are received through an async channel.
//! * Job is only consumed from the channel once there is a prover available.
//! * Assigned jobs are added to `ProverJobMap` immediately.
//! * Provers request work via [`pick_next_job`]:
//!     * If there is an already assigned job that has timed out, it is reassigned.
//!     * Otherwise, the next job from inbound is assigned and inserted into `ProverJobMap`.
//! * Fake provers can call [`pick_next_job_with_min_age`] to avoid taking fresh items,
//!   letting real provers race first.
//! * When any proof is submitted (real or fake):
//!     * It is enqueued to the ordered committer as `BatchEnvelope<FriProof>`.
//!     * It is removed from `ProverJobMap` so the map cannot grow without bounds.

use crate::model::batches::{BatchEnvelope, FriProof, ProverInput};
use crate::prover_api::metrics::PROVER_METRICS;
use crate::prover_api::proof_verifier;
use crate::prover_api::prover_job_map::ProverJobMap;
use crate::util::peekable_receiver::PeekableReceiver;
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
#[derive(Debug)]
pub struct ProverJobManager {
    // == state ==
    assigned_jobs: ProverJobMap,
    // == plumbing ==
    // inbound
    inbound: Mutex<PeekableReceiver<BatchEnvelope<ProverInput>>>,
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
            assigned_jobs: jobs,
            inbound: Mutex::new(PeekableReceiver::new(batches_for_prove_receiver)),
            batches_with_proof_sender,
        }
    }

    /// Picks the **smallest** batch number that is either **pending** (from inbound)
    /// or whose assignment has **timed‑out** (from the assigned map).
    ///
    /// If `min_inbound_age` is provided, will **not** consume a fresh inbound head item
    /// whose trace age is **younger** than this threshold; in that case returns `None`
    /// to let real provers race first.
    pub fn pick_next_job(&self, min_inbound_age: Duration) -> Option<(u64, ProverInput)> {
        // 1) Prefer a timed-out reassignment
        if let Some(batch_envelope) = self.assigned_jobs.pick_timed_out_job() {
            let batch_number = batch_envelope.batch_number();
            let prover_input = batch_envelope.data;
            return Some((batch_number, prover_input));
        }

        // 2) Otherwise, consume one item from inbound - if it meets the age gate.
        if let Ok(mut rx) = self.inbound.try_lock() {
            let old_enough = rx.peek_with(|env| env.trace.last_stage_age() >= min_inbound_age);
            if old_enough.is_none() || !old_enough.unwrap() {
                // no element in Inbound or it's not old enough
                return None;
            }

            match rx.try_recv() {
                Ok(env) => {
                    let batch_number = env.batch_number();
                    let prover_input = env.data.clone();
                    tracing::info!(
                        batch_number,
                        assigned_jobs_count = self.assigned_jobs.len(),
                        last_stage_age = ?env.trace.last_stage_age(),
                        ?min_inbound_age,
                        "received new job from inbound"
                    );
                    self.assigned_jobs.insert(env);
                    Some((batch_number, prover_input))
                }
                Err(_) => None,
            }
        } else {
            // in fact, we could wait for mutex to unlock -
            // but we return early and let prover to poll again
            tracing::debug!("inbound receiver is contended; returning None");
            None
        }
    }

    /// Submit a **real** proof provided by an external prover. On success the entry
    /// is removed so the map cannot grow without bounds.
    pub async fn submit_proof(
        &self,
        batch_number: u64,
        proof_bytes: Vec<u8>,
        prover_id: &str,
    ) -> Result<(), SubmitError> {
        // Snapshot the assigned job entry (if any).
        let assigned = match self.assigned_jobs.get(batch_number) {
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

        let envelope = BatchEnvelope {
            batch: assigned.batch_envelope.batch,
            data: FriProof::Real(proof_bytes),
            trace: assigned.batch_envelope.trace.with_stage("proof_generated"),
        };
        self.batches_with_proof_sender
            .send(envelope)
            .await
            .map_err(|err| SubmitError::Other(err.to_string()))?;

        // Remove the job from the assigned map. If already removed due to a race
        // (another submit won), we treat it as success to keep the API idempotent.
        if self.assigned_jobs.remove(batch_number).is_none() {
            tracing::warn!(
                batch_number,
                "Proof persisted; job already removed (racing submit)"
            );
        } else {
            tracing::info!(batch_number, "Real proof accepted");
        }
        Ok(())
    }

    /// Submit a **fake** proof on behalf of a fake prover worker.
    /// On success the entry is removed from the assigned map.
    pub async fn submit_fake_proof(
        &self,
        batch_number: u64,
        prover_id: &str,
    ) -> Result<(), SubmitError> {
        // Snapshot the assigned job entry (if any).
        let assigned = match self.assigned_jobs.get(batch_number) {
            Some(e) => e,
            None => return Err(SubmitError::UnknownJob(batch_number)),
        };

        // Metrics: observe time since the last assignment.
        let prove_time = assigned.assigned_at.elapsed();
        let label: &'static str = Box::leak(prover_id.to_owned().into_boxed_str());
        PROVER_METRICS.prove_time[&label].observe(prove_time);
        PROVER_METRICS.prove_time_per_tx[&label]
            .observe(prove_time / assigned.batch_envelope.batch.tx_count as u32);

        // No verification / deserialization — we emit a fake proof.
        let envelope = BatchEnvelope {
            batch: assigned.batch_envelope.batch,
            data: FriProof::Fake,
            trace: assigned
                .batch_envelope
                .trace
                .with_stage("fake_proof_generated"),
        };

        // Enqueue first, then remove from map.
        self.batches_with_proof_sender
            .send(envelope)
            .await
            .map_err(|err| SubmitError::Other(err.to_string()))?;

        let _ = self.assigned_jobs.remove(batch_number);
        tracing::info!(batch_number, "Fake proof accepted");
        Ok(())
    }

    pub fn status(&self) -> Vec<JobState> {
        self.assigned_jobs.status()
    }
}
