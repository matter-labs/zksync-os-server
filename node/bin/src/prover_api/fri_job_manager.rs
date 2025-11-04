//! Concurrent in‑memory queue for FRI prover work.
//!
//! * Incoming jobs are received through an async channel.
//! * Job is only consumed from the channel once there is a prover available.
//! * Assigned jobs are added to `ProverJobMap` immediately.
//! * Provers request work via [`pick_next_job`]:
//!     * If there is an already assigned job that has timed out, it is reassigned.
//!     * Otherwise, the next job from inbound is assigned and inserted into `ProverJobMap`.
//! * Fake provers call [`pick_next_job`] with a `min_age` param to avoid taking fresh items,
//!   letting real provers race first.
//! * When any proof is submitted (real or fake):
//!     * It is enqueued to the ordered committer as `BatchEnvelope<FriProof>`.
//!     * It is removed from `ProverJobMap` so the map cannot grow without bounds.
//!
//! `ComponentStateLatencyTracker`: Only tracks `Processing` / `WaitingSend` states

use crate::prover_api::fri_proof_verifier;
use crate::prover_api::metrics::{PROVER_METRICS, ProverStage, ProverType};
use crate::prover_api::proof_storage::{ProofStorage, StoredFailedProof};
use crate::prover_api::prover_job_map::ProverJobMap;
use alloy::primitives::Bytes;
use itertools::MinMaxResult::MinMax;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc::Permit;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Mutex, mpsc};
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof, ProverInput, RealFriProof};
use zksync_os_multivm::{ExecutionVersion, proving_run_execution_version};
use zksync_os_observability::{
    ComponentStateHandle, ComponentStateReporter, GenericComponentState,
};
use zksync_os_pipeline::PeekableReceiver;

#[derive(Error, Debug)]
pub enum SubmitError {
    #[error("FRI proof verification error")]
    FriProofVerificationError {
        expected_hash_u32s: [u32; 8],
        proof_final_register_values: [u32; 16],
    },
    #[error("batch {0} is not known to the server")]
    UnknownJob(u64),
    #[error("deserialization failed: {0:?}")]
    DeserializationFailed(bincode::error::DecodeError),
    // server execution version, prover execution version
    #[error("execution error mismatch - server expects {0:?}, but got {1:?} from prover")]
    ExecutionVersionMismatch(ExecutionVersion, ExecutionVersion),
    #[error("internal error: {0}")]
    Other(String),
}

/// A FRI proof that failed verification, stored for debugging purposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedFriProof {
    pub batch_number: u64,
    pub last_block_timestamp: u64,
    pub expected_hash_u32s: [u32; 8],
    pub proof_final_register_values: [u32; 16],
    // TODO: migrate to String, once legacy is deprecated
    pub vk_hash: Option<String>,
    pub proof_bytes: Bytes,
}

#[derive(Debug, Serialize)]
pub struct FriJob {
    pub batch_number: u64,
    pub vk_hash: String,
}

#[derive(Debug, Serialize)]
pub struct JobState {
    pub fri_job: FriJob,
    pub assigned_seconds_ago: u64,
}

// TODO: remove, once legacy is deprecated
#[derive(Debug, Serialize)]
pub struct JobStateLegacy {
    pub batch_number: u64,
    pub assigned_seconds_ago: u64,
}

impl From<JobState> for JobStateLegacy {
    fn from(state: JobState) -> JobStateLegacy {
        JobStateLegacy {
            batch_number: state.fri_job.batch_number,
            assigned_seconds_ago: state.assigned_seconds_ago,
        }
    }
}

/// Thread-safe queue for FRI prover work.
/// Holds up to `max_assigned_batch_range` batches in `assigned_jobs`.
pub struct FriJobManager {
    // == state ==
    assigned_jobs: ProverJobMap,
    // == plumbing ==
    // inbound
    inbound: Mutex<PeekableReceiver<BatchEnvelope<ProverInput>>>,
    // outbound
    batches_with_proof_sender: mpsc::Sender<BatchEnvelope<FriProof>>,
    // == storage ==
    proof_storage: ProofStorage,
    // == config ==
    max_assigned_batch_range: usize,
    // == metrics ==
    latency_tracker: ComponentStateHandle<GenericComponentState>,
}

impl FriJobManager {
    pub fn new(
        batches_for_prove_receiver: mpsc::Receiver<BatchEnvelope<ProverInput>>,
        batches_with_proof_sender: mpsc::Sender<BatchEnvelope<FriProof>>,
        proof_storage: ProofStorage,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> Self {
        let jobs = ProverJobMap::new(assignment_timeout);
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "fri_job_manager",
            GenericComponentState::ProcessingOrWaitingRecv,
        );
        Self {
            assigned_jobs: jobs,
            inbound: Mutex::new(PeekableReceiver::new(batches_for_prove_receiver)),
            batches_with_proof_sender,
            proof_storage,
            max_assigned_batch_range,
            latency_tracker,
        }
    }

    /// Peek a batch data for a given batch number
    pub fn peek_batch_data(&self, batch_number: u64) -> Option<(&str, ProverInput)> {
        match self.assigned_jobs.get_batch_data(batch_number) {
            Some((vk_hash, prover_input)) => {
                tracing::info!("Batch data is peeked for batch number {batch_number}");
                Some((vk_hash, prover_input))
            }
            None => {
                tracing::debug!(
                    "Trying to peek batch number {batch_number} that is not present in assigned_jobs"
                );
                None
            }
        }
    }

    /// Picks the **smallest** batch number that is either **pending** (from inbound)
    /// or whose assignment has **timed‑out** (from the assigned map).
    ///
    /// If `min_inbound_age` is provided, will **not** consume a fresh inbound head item
    /// whose trace age is **younger** than this threshold; in that case returns `None`
    /// to let real provers race first.
    ///
    /// `min_inbound_age` is used for fake provers to avoid taking fresh items,
    /// letting real provers race first.
    pub fn pick_next_job(&self, min_inbound_age: Duration) -> Option<(FriJob, ProverInput)> {
        // 1) Prefer a timed-out reassignment
        if let Some((fri_job, prover_input)) = self.assigned_jobs.pick_timed_out_job() {
            tracing::info!(
                fri_job.batch_number,
                fri_job.vk_hash,
                assigned_jobs_count = self.assigned_jobs.len(),
                ?min_inbound_age,
                "Assigned a timed out job"
            );
            return Some((fri_job, prover_input));
        }

        if let MinMax(min, max) = self.assigned_jobs.minmax_assigned_batch_number()
            && max - min >= self.max_assigned_batch_range as u64
        {
            // fresh assignments are not allowed when there are too many assigned jobs
            tracing::debug!(
                assigned_jobs_count = self.assigned_jobs.len(),
                max_assigned_batch_range = self.max_assigned_batch_range,
                "too many assigned jobs; returning None"
            );
            return None;
        }

        // 2) Otherwise, consume one item from inbound - if it meets the age gate.
        // take a lock on the inbound channel - only one thread can receive messages at a time
        if let Ok(mut rx) = self.inbound.try_lock() {
            let old_enough =
                rx.peek_with(|env| env.latency_tracker.current_stage_age() >= min_inbound_age);
            if old_enough != Some(true) {
                // no element in Inbound or it's not old enough
                return None;
            }

            match rx.try_recv() {
                Ok(env) => {
                    let env = env.with_stage(BatchExecutionStage::FriProverPicked);
                    let prover_input = env.data.clone();
                    let proving_execution_version =
                        proving_run_execution_version(env.batch.execution_version);
                    let fri_job = FriJob {
                        batch_number: env.batch_number(),
                        vk_hash: proving_execution_version.vk_hash().to_string(),
                    };
                    tracing::info!(
                        fri_job.batch_number,
                        assigned_jobs_count = self.assigned_jobs.len(),
                        ?min_inbound_age,
                        "Assigned a new job from inbound channel"
                    );
                    self.assigned_jobs.insert(env);
                    Some((fri_job, prover_input))
                }
                Err(_) => None,
            }
        } else {
            // in fact, we could wait for mutex to unlock -
            // but we return early and let prover poll again
            tracing::trace!("inbound receiver is contended; returning None");
            None
        }
    }

    /// Submit a **real** proof provided by an external prover. On success the entry
    /// is removed so the map cannot grow without bounds.
    pub async fn submit_proof(
        &self,
        batch_number: u64,
        proof_bytes: Bytes,
        // TODO: migrate to ExecutionVersion, once legacy is deprecated
        execution_version: Option<ExecutionVersion>,
        prover_id: &str,
    ) -> Result<(), SubmitError> {
        // Snapshot the assigned job entry (if any).
        let (assigned_at, batch_metadata) = match self.assigned_jobs.get(batch_number) {
            Some(e) => e,
            None => return Err(SubmitError::UnknownJob(batch_number)),
        };

        // Prover should generate the proof with VK received from server. These must always match.
        // If they don't, proof won't be accepted, validation will fail, therefore it's pointless to proceed.
        //
        // This should never happen, but we double-check to guarantee it's the case.
        //
        // NOTE: We don't check the actual values, but the value that server believes the prove should use.
        // NOTE2: Checking only if prover provided VK version - legacy clients will not provide it
        if let Some(exec_version) = execution_version {
            // should never panic
            let server_execution_version =
                proving_run_execution_version(batch_metadata.execution_version);
            if server_execution_version != exec_version {
                return Err(SubmitError::ExecutionVersionMismatch(
                    server_execution_version,
                    exec_version,
                ));
            }
        }

        // Deserialize and verify using metadata from the batch.
        let program_proof =
            bincode::serde::decode_from_slice(&proof_bytes, bincode::config::standard())
                .map_err(|err| {
                    tracing::warn!(batch_number, ?err, "Failed to deserialize proof");
                    SubmitError::DeserializationFailed(err)
                })?
                .0;

        if let Err(SubmitError::FriProofVerificationError {
            expected_hash_u32s,
            proof_final_register_values,
        }) = fri_proof_verifier::verify_fri_proof(
            batch_metadata.previous_stored_batch_info.state_commitment,
            batch_metadata.batch_info.clone().into_stored(),
            program_proof,
        ) {
            tracing::warn!(
                batch_number,
                expected = ?expected_hash_u32s,
                actual = ?proof_final_register_values,
                "Proof verification failed",
            );

            // Persist the failed proof with some information about the batch for debugging
            let failed_proof = FailedFriProof {
                batch_number,
                last_block_timestamp: batch_metadata.batch_info.commit_info.last_block_timestamp,
                expected_hash_u32s,
                proof_final_register_values,
                vk_hash: Some(batch_metadata.verification_key_hash().to_string()),
                proof_bytes,
            };

            if let Err(save_err) = self
                .proof_storage
                .save_failed_proof(&StoredFailedProof { failed_proof })
                .await
            {
                tracing::error!(
                    batch_number,
                    ?save_err,
                    "Failed to persist failed proof for debugging",
                );
            } else {
                tracing::info!(batch_number, prover_id, "Failed proof saved for debugging",);
            }

            return Err(SubmitError::FriProofVerificationError {
                expected_hash_u32s,
                proof_final_register_values,
            });
        }
        // Now we know that the proof is valid.

        // Metrics: observe time since the last assignment.
        let prove_time = assigned_at.elapsed();
        let label: &'static str = Box::leak(prover_id.to_owned().into_boxed_str());

        PROVER_METRICS.prove_time[&(ProverStage::Fri, ProverType::Real, label)].observe(prove_time);
        PROVER_METRICS.prove_time_per_tx[&(ProverStage::Fri, ProverType::Real, label)]
            .observe(prove_time / batch_metadata.tx_count as u32);

        // We want to ensure we can send the result downstream before we remove the job
        let permit = self.try_reserve_permit_downstream()?;

        // Remove the job from the assigned map. If already removed due to a race
        // (another submit won), we treat it as a success to keep the API idempotent.
        let Some(removed_job) = self.assigned_jobs.remove(batch_number) else {
            tracing::warn!(
                batch_number,
                "Proof persisted; job already removed (racing submit)"
            );
            return Ok(());
        };
        tracing::info!(batch_number, "Real proof accepted");

        // get execution version from prover, if available, otherwise fallback
        let execution_version = if let Some(execution_version) = execution_version {
            proving_run_execution_version(execution_version as u32) as u32
        } else {
            proving_run_execution_version(batch_metadata.execution_version) as u32
        };

        // Prepare the envelope and send it downstream.
        let proof = RealFriProof::V2 {
            proof: proof_bytes,
            proving_execution_version: execution_version,
        };
        let envelope = removed_job
            .batch_envelope
            .with_data(FriProof::Real(proof))
            .with_stage(BatchExecutionStage::FriProvedReal);

        permit.send(envelope);

        Ok(())
    }

    /// Submit a **fake** proof on behalf of a fake prover worker.
    /// Entry is removed from the assigned map.
    pub async fn submit_fake_proof(
        &self,
        batch_number: u64,
        prover_id: &str,
    ) -> Result<(), SubmitError> {
        // We want to ensure we can send the result downstream before we remove the job
        let permit = self.try_reserve_permit_downstream()?;

        // Downstream capacity availably - we remove the job from `assigned_jobs`.
        // Fake proofs are always valid, so there is no chance that we want to reschedule it
        let assigned = match self.assigned_jobs.remove(batch_number) {
            Some(e) => e,
            None => return Err(SubmitError::UnknownJob(batch_number)),
        };

        // Metrics: observe time since the last assignment.
        let prove_time = assigned.assigned_at.elapsed();
        let label: &'static str = Box::leak(prover_id.to_owned().into_boxed_str());

        PROVER_METRICS.prove_time[&(ProverStage::Fri, ProverType::Fake, label)].observe(prove_time);
        PROVER_METRICS.prove_time_per_tx[&(ProverStage::Fri, ProverType::Fake, label)]
            .observe(prove_time / assigned.batch_envelope.batch.tx_count as u32);

        // No verification / deserialization — we emit a fake proof.

        let envelope = assigned
            .batch_envelope
            .with_data(FriProof::Fake)
            .with_stage(BatchExecutionStage::FriProvedFake);

        permit.send(envelope);

        tracing::info!(batch_number, "Fake proof accepted");
        Ok(())
    }
    fn try_reserve_permit_downstream(
        &self,
    ) -> Result<Permit<BatchEnvelope<FriProof>>, SubmitError> {
        Ok(match self.batches_with_proof_sender.try_reserve() {
            Ok(permit) => {
                self.set_status(GenericComponentState::ProcessingOrWaitingRecv);
                permit
            }
            Err(TrySendError::Full(_)) => {
                self.set_status(GenericComponentState::WaitingSend);
                return Err(SubmitError::Other("downstream backpressure".to_string()));
            }
            Err(TrySendError::Closed(_)) => {
                return Err(SubmitError::Other("server is shutting down".to_string()));
            }
        })
    }

    pub fn status(&self) -> Vec<JobState> {
        self.assigned_jobs.status()
    }

    fn set_status(&self, status: GenericComponentState) {
        self.latency_tracker.enter_state(status);
    }
}

impl std::fmt::Debug for FriJobManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FriJobManager")
            .field("assigned_jobs_len", &self.assigned_jobs.len())
            .field("max_assigned_batch_range", &self.max_assigned_batch_range)
            .finish()
    }
}
