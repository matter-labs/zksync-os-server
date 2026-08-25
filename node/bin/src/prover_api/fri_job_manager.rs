//! Concurrent in‑memory queue for FRI prover work.
//!
//! * Incoming jobs are received via `add_job`.
//!   No more than `max_assigned_batch_range` batch span is accepted
//! * Assigned jobs are added to `ProverJobMap` immediately.
//! * Provers request work via [`pick_next_job`]:
//!     * If there is an already assigned job that has timed out, it is reassigned.
//!     * Otherwise, the next job from inbound is assigned and inserted into `ProverJobMap`.
//! * Fake provers call [`pick_next_job`] with a `min_age` param to avoid taking fresh items,
//!   letting real provers race first.
//! * When any proof is submitted (real or fake):
//!     * It is removed from `ProverJobMap`
//!     * It is enqueued to the ordered committer as `SignedBatchEnvelope<FriProof>`.
//!

use crate::prover_api::fri_proof_verifier;
use crate::prover_api::metrics::{ProverStage, ProverType};
use crate::prover_api::proof_storage::ProofStorage;
use crate::prover_api::prover_job_map::ProverJobMap;
use alloy::primitives::Bytes;
use jsonrpsee::core::Serialize;
use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use zksync_os_batch_types::batcher_model::{
    BatchMetadata, FriProof, ProverInput, RealFriProof, SignedBatchEnvelope,
};
use zksync_os_batcher_metrics::BatchExecutionStage;
use zksync_os_types::{
    FriProofConfiguration, ProtocolSemanticVersion, ProvingStackConfiguration,
    require_proving_config,
};

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
    #[error(
        "verification key mismatch - server expects {expected}, but prover submitted {submitted}"
    )]
    VerificationKeyMismatch { expected: String, submitted: String },
    #[error("server is shutting down")]
    ShuttingDown,
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
    pub vk_hash: String,
    pub proof_bytes: Bytes,
}

#[derive(Clone, Debug, Serialize)]
pub struct FriJob {
    pub batch_number: u64,
    pub vk_hash: String,
}

#[derive(Debug, Serialize)]
pub struct JobState {
    pub fri_job: FriJob,
    pub added_seconds_ago: u64,
    pub assigned_seconds_ago: Option<u64>,
    pub assigned_to_prover_id: Option<String>,
    pub current_attempt: usize,
}

#[derive(Debug)]
pub struct FriJobManager {
    // == state ==
    jobs: ProverJobMap<ProverInput>,
    // outbound
    batches_with_proof_sender: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
    // == storage ==
    proof_storage: ProofStorage,
}

impl FriJobManager {
    pub fn new(
        batches_with_proof_sender: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
        proof_storage: ProofStorage,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> Self {
        let jobs = ProverJobMap::<ProverInput>::new(
            assignment_timeout,
            max_assigned_batch_range,
            ProverStage::Fri,
        );
        Self {
            jobs,
            batches_with_proof_sender,
            proof_storage,
        }
    }

    /// Adds a pending job to the queue.
    /// Awaits if the queue is full (ProverJobMap.max_assigned_batch_range).
    pub async fn add_job(&self, batch_envelope: SignedBatchEnvelope<ProverInput>) {
        self.jobs.add_job(batch_envelope).await
    }

    /// Peek batch data for a given batch number
    pub async fn peek_batch_data(&self, batch_number: u64) -> Option<(&str, ProverInput)> {
        match self.jobs.get_prover_input(batch_number).await {
            Some((vk_hash, prover_input)) => {
                tracing::info!("Batch data is peeked for batch number {batch_number}");
                Some((vk_hash, prover_input))
            }
            None => {
                tracing::debug!(
                    "Trying to peek batch number {batch_number} that is not present in the queue"
                );
                None
            }
        }
    }

    /// Picks the oldest batch that is either pending and old enough
    /// or whose assignment has timed‑out.
    ///
    /// `min_age` is used for fake provers to avoid taking fresh items,
    /// letting real provers race first.
    ///
    /// `supported_vk_hashes` restricts assignment to batches with these verification keys;
    /// `None` means the prover declared nothing and any batch qualifies.
    pub async fn pick_next_job(
        &self,
        min_age: Duration,
        prover_id: String,
        supported_vk_hashes: Option<&[&'static str]>,
    ) -> Option<(FriJob, ProverInput)> {
        self.jobs
            .pick_job(min_age, &prover_id, |job| {
                supported_vk_hashes.is_none_or(|hashes| {
                    hashes
                        .iter()
                        .any(|&hash| hash == job.metadata.verification_key_hash())
                })
            })
            .await
    }

    /// Submit a **real** proof provided by an external prover.
    /// On success the entry is removed from the assigned map.
    pub async fn submit_proof(
        &self,
        batch_number: u64,
        proof_bytes: Bytes,
        submitted_vk_hash: &str,
        prover_id: &str,
    ) -> Result<(), SubmitError> {
        // Snapshot the assigned job entry (if any).
        let batch_metadata = match self.jobs.get_job_batch_metadata(batch_number).await {
            Some(e) => e,
            None => return Err(SubmitError::UnknownJob(batch_number)),
        };

        let verdict = async {
            let proving_config = validate_submitted_verification_key(
                &batch_metadata.batch_info.protocol_version,
                submitted_vk_hash,
            )?;
            self.verify_proof(
                proving_config,
                &batch_metadata,
                &proof_bytes,
                batch_number,
                prover_id,
            )
            .await
        }
        .await;
        if let Err(err) = verdict {
            // Definitive rejection: release the assignment so the job can be re-picked
            // immediately instead of waiting out the assignment timeout (which is set
            // to many hours for slow CPU provers).
            self.jobs.unassign_job(batch_number, prover_id).await;
            return Err(err);
        }

        // We want to ensure we can send the result downstream before we remove the job from queue
        let permit = self.try_reserve_permit_downstream()?;

        // Remove the job from the assigned map.
        let Some(removed_job) = self
            .jobs
            .complete_job(batch_number, ProverType::Real, prover_id)
            .await
        else {
            // If already removed due to a race
            // (another submit won), we still return success to keep the API idempotent.
            tracing::warn!(
                batch_number,
                prover_id,
                "Job already removed (racing submit)"
            );
            return Ok(());
        };

        // Prepare the envelope and send it downstream.
        let proof = RealFriProof { proof: proof_bytes };
        let envelope = removed_job
            .with_data(FriProof::Real(proof))
            .with_stage(BatchExecutionStage::FriProvedReal);

        permit.send(envelope);

        Ok(())
    }

    /// Verifies the proof and handles failed proofs by saving them for debugging.
    /// Returns Ok(()) if the proof is valid, or an error if verification fails.
    async fn verify_proof(
        &self,
        proving_config: &'static ProvingStackConfiguration,
        batch_metadata: &BatchMetadata,
        proof_bytes: &Bytes,
        batch_number: u64,
        prover_id: &str,
    ) -> Result<(), SubmitError> {
        // Deserialization + cryptographic verification are CPU-heavy (seconds of work) -
        // run them on a blocking thread so prover API requests don't stall the runtime.
        // `spawn_blocking` also catches panics that escape the verifiers' own `catch_unwind`.
        let verify_result = tokio::task::spawn_blocking({
            let batch_metadata = batch_metadata.clone();
            let proof_bytes = proof_bytes.clone();
            move || {
                Self::verify_proof_blocking(
                    proving_config,
                    &batch_metadata,
                    &proof_bytes,
                    batch_number,
                )
            }
        })
        .await;

        let result = match verify_result {
            Ok(result) => result,
            Err(join_error) if join_error.is_panic() => {
                tracing::error!(
                    batch_number,
                    prover_id,
                    %join_error,
                    "proof verification panicked; rejecting the proof"
                );
                // The verifier died before producing register values; still report the
                // expected hash so the persisted proof stays diagnosable.
                let expected_hash_u32s = fri_proof_verifier::expected_public_input_registers(
                    proving_config,
                    batch_metadata,
                )
                .unwrap_or([0u32; 8]);
                Err(SubmitError::FriProofVerificationError {
                    expected_hash_u32s,
                    proof_final_register_values: [0u32; 16],
                })
            }
            Err(join_error) => {
                return Err(SubmitError::Other(format!(
                    "proof verification task failed: {join_error}"
                )));
            }
        };
        match result {
            Ok(()) => Ok(()),
            Err(SubmitError::FriProofVerificationError {
                expected_hash_u32s,
                proof_final_register_values,
            }) => {
                tracing::warn!(
                    batch_number,
                    expected = ?expected_hash_u32s,
                    actual = ?proof_final_register_values,
                    "Proof verification failed",
                );

                // Persist the failed proof with some information about the batch for debugging
                let failed_proof = FailedFriProof {
                    batch_number,
                    last_block_timestamp: batch_metadata
                        .batch_info
                        .commit_info
                        .last_block_timestamp,
                    expected_hash_u32s,
                    proof_final_register_values,
                    vk_hash: proving_config.verification_key_hash.to_string(),
                    proof_bytes: proof_bytes.clone(),
                };

                if let Err(save_err) = self.proof_storage.save_failed_proof(&failed_proof).await {
                    tracing::error!(
                        batch_number,
                        ?save_err,
                        "Failed to persist failed proof for debugging",
                    );
                } else {
                    tracing::info!(batch_number, prover_id, "Failed proof saved for debugging",);
                }

                Err(SubmitError::FriProofVerificationError {
                    expected_hash_u32s,
                    proof_final_register_values,
                })
            }
            // Any other error (deserialization, unsupported version, ...) must reject the
            // submission too - falling through here would accept an unverified proof.
            Err(err) => Err(err),
        }
    }

    /// Deserializes and cryptographically verifies the proof.
    /// CPU-heavy and may panic on malformed input - always call via `spawn_blocking`
    /// (see `verify_proof`).
    fn verify_proof_blocking(
        proving_config: &'static ProvingStackConfiguration,
        batch_metadata: &BatchMetadata,
        proof_bytes: &Bytes,
        batch_number: u64,
    ) -> Result<(), SubmitError> {
        let expected_hash_u32s =
            fri_proof_verifier::expected_public_input_registers(proving_config, batch_metadata)?;
        match proving_config.fri {
            FriProofConfiguration::PreV8 => {
                tracing::debug!("Using 0.5.2 proof verifier for batch {}", batch_number);
                let program_proof =
                    bincode::serde::decode_from_slice(proof_bytes, bincode::config::standard())
                        .map_err(|err| {
                            tracing::warn!(batch_number, ?err, "Failed to deserialize proof");
                            SubmitError::DeserializationFailed(err)
                        })?
                        .0;
                fri_proof_verifier::verify_fri_proof(
                    expected_hash_u32s,
                    program_proof,
                    batch_number,
                )
            }
            FriProofConfiguration::V8 {
                application_end_params,
            } => {
                tracing::debug!("Using V8 proof verifier for batch {}", batch_number);
                let program_proof: execution_utils::unrolled::UnrolledProgramProof =
                    bincode::serde::decode_from_slice(proof_bytes, bincode::config::standard())
                        .map_err(|err| {
                            tracing::warn!(batch_number, ?err, "Failed to deserialize V8 proof");
                            SubmitError::DeserializationFailed(err)
                        })?
                        .0;
                fri_proof_verifier::verify_fri_proof_v8(
                    expected_hash_u32s,
                    &program_proof,
                    batch_number,
                    application_end_params,
                )
            }
        }
    }

    /// Submit a **fake** proof on behalf of a fake prover worker.
    /// Entry is removed from the assigned map.
    pub async fn submit_fake_proof(
        &self,
        batch_number: u64,
        prover_id: &'static str,
    ) -> Result<(), SubmitError> {
        // We want to ensure we can send the result downstream before we remove the job
        let permit = self.try_reserve_permit_downstream()?;

        // Downstream has capacity - we remove the job from `assigned_jobs`.
        let assigned = match self
            .jobs
            .complete_job(batch_number, ProverType::Fake, prover_id)
            .await
        {
            Some(e) => e,
            None => return Err(SubmitError::UnknownJob(batch_number)),
        };

        let envelope = assigned
            .with_data(FriProof::Fake)
            .with_stage(BatchExecutionStage::FriProvedFake);

        permit.send(envelope);
        Ok(())
    }

    pub async fn status(&self) -> Vec<JobState> {
        self.jobs.status().await
    }

    fn try_reserve_permit_downstream(
        &self,
    ) -> Result<mpsc::Permit<'_, SignedBatchEnvelope<FriProof>>, SubmitError> {
        match self.batches_with_proof_sender.try_reserve() {
            Ok(permit) => Ok(permit),
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err(SubmitError::Other("downstream backpressure".to_string()))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SubmitError::ShuttingDown),
        }
    }
}

fn validate_submitted_verification_key(
    protocol_version: &ProtocolSemanticVersion,
    submitted_vk_hash: &str,
) -> Result<&'static ProvingStackConfiguration, SubmitError> {
    let proving_config = require_proving_config(protocol_version, "FRI proof submission")
        .map_err(|err| SubmitError::Other(err.to_string()))?;
    if proving_config.verification_key_hash != submitted_vk_hash {
        return Err(SubmitError::VerificationKeyMismatch {
            expected: proving_config.verification_key_hash.to_string(),
            submitted: submitted_vk_hash.to_string(),
        });
    }
    Ok(proving_config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_types::proving_registry;

    #[test]
    fn proof_submission_rejects_vk_not_registered_for_batch_protocol() {
        let registry = proving_registry();
        let (batch_entry, submitted_entry) = registry
            .iter()
            .find_map(|batch_entry| {
                registry
                    .iter()
                    .find(|submitted_entry| {
                        submitted_entry.configuration.verification_key_hash
                            != batch_entry.configuration.verification_key_hash
                    })
                    .map(|submitted_entry| (batch_entry, submitted_entry))
            })
            .expect("production proving registry must contain distinct verification keys");

        let error = validate_submitted_verification_key(
            &batch_entry.protocol_version,
            submitted_entry.configuration.verification_key_hash,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SubmitError::VerificationKeyMismatch { expected, submitted }
                if expected == batch_entry.configuration.verification_key_hash
                    && submitted == submitted_entry.configuration.verification_key_hash
        ));
    }
}
