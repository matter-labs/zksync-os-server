//! Concurrent in‑memory queue for FRI prover work.
//!
//! * Incoming jobs are received through an async channel (see
//!   [`ProverJobManager::listen_for_batch_jobs`]).
//! * Provers request work via [`pick_next_job`], which always returns the
//!   smallest block number that is **pending** or has **timed‑out**.
//! * When a proof is submitted and verified, the job is removed so the map
//!   cannot grow unbounded.
//!

use std::time::{Duration, Instant};

use crate::model::BatchJob;
use crate::prover_api::metrics::PROVER_METRICS;
use crate::prover_api::proof_storage::ProofStorage;
use air_compiler_cli::prover_utils::{
    generate_oracle_data_from_metadata_and_proof_list, proof_list_and_metadata_from_program_proof,
};
use alloy::primitives::B256;
use dashmap::DashMap;
use execution_utils::ProgramProof;
use itertools::Itertools;
use serde::Serialize;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::Receiver;
use zk_os_basic_system::system_implementation::system::BatchPublicInput;
use zksync_os_l1_sender::commitment::{CommitBatchInfo, StoredBatchInfo};
// ───────────── Error types ─────────────

#[derive(Error, Debug)]
pub enum SubmitError {
    #[error("proof did not pass verification")]
    VerificationFailed,
    #[error("block {0} is not known to the server")]
    UnknownJob(u64),
    #[error("deserialization failed: {0:?}")]
    DeserializationFailed(bincode_v1::Error),
    #[error("internal error: {0}")]
    Other(String),
}

// ───────────── Internal state ─────────────

#[derive(Debug, Clone)]
struct JobEntry {
    prover_input: Vec<u32>,
    previous_state_commitment: B256,
    commit_batch_info: CommitBatchInfo,
    status: JobStatus,
}

#[derive(Debug, Clone)]
enum JobStatus {
    Pending,
    Assigned { assigned_at: Instant },
}

/// Public view of one job’s state.
#[derive(Debug, Serialize)]
pub struct JobState {
    pub block_number: u64,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prover_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_seconds_ago: Option<u32>,
}
// ───────────── Public API ─────────────

pub struct ProverJobManager {
    jobs: DashMap<u64, JobEntry>,
    proof_storage: ProofStorage,
    assignment_timeout: Duration,
    // applies backpressure when `jobs` reaches this number
    max_jobs_count: usize,
    /// Ensures that only one thread performs the *scan* -> *assign* sequence at a
    /// time, avoiding racy double‑assignments while leaving inserts and
    /// submissions fully concurrent.
    pick_lock: parking_lot::Mutex<()>,
}

impl ProverJobManager {
    pub fn new(
        proof_storage: ProofStorage,
        assignment_timeout: Duration,
        max_jobs_count: usize,
    ) -> Self {
        Self {
            jobs: DashMap::new(),
            proof_storage,
            assignment_timeout,
            max_jobs_count,
            pick_lock: parking_lot::Mutex::new(()),
        }
    }

    pub async fn listen_for_batch_jobs(
        self: Arc<Self>,
        mut rx: Receiver<BatchJob>,
    ) -> anyhow::Result<()> {
        while let Some(job) = rx.recv().await {
            while self.jobs.len() >= self.max_jobs_count {
                // wait for `jobs` to be empty
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            self.jobs.insert(
                job.block_number,
                JobEntry {
                    prover_input: job.prover_input,
                    previous_state_commitment: job.previous_state_commitment,
                    commit_batch_info: job.commit_batch_info,
                    status: JobStatus::Pending,
                },
            );
        }
        Ok(())
    }

    /// Picks the **smallest** block number that is either **pending** or whose
    /// assignment has **timed‑out**. Returns its `(block, prover_input)`.
    pub fn pick_next_job(&self) -> Option<(u64, Vec<u32>)> {
        let _guard = self.pick_lock.lock();
        let now = Instant::now();

        // Single scan to locate the minimal eligible key.
        let candidate = self
            .jobs
            .iter()
            .filter_map(|entry| {
                let key = *entry.key();
                match &entry.status {
                    JobStatus::Pending => Some(key),
                    JobStatus::Assigned { assigned_at, .. }
                        if now.duration_since(*assigned_at) > self.assignment_timeout =>
                    {
                        Some(key)
                    }
                    _ => None,
                }
            })
            .min();

        if let Some(block) = candidate {
            if let Some(mut entry) = self.jobs.get_mut(&block) {
                let input = entry.prover_input.clone();
                entry.status = JobStatus::Assigned { assigned_at: now };
                return Some((block, input));
            }
        }
        None
    }

    /// Called by the HTTP handler after verifying a proof. On success the entry
    /// is removed so the map cannot grow without bounds.
    pub fn submit_proof(
        &self,
        block: u64,
        proof: Vec<u8>,
        prover_id: &str,
    ) -> Result<(), SubmitError> {
        let entry_ref = self
            .jobs
            .get(&block)
            .ok_or(SubmitError::UnknownJob(block))?;
        let entry = entry_ref.value();

        match &entry.status {
            JobStatus::Assigned { assigned_at } => {
                let prove_time = assigned_at.elapsed();
                let label: &'static str = Box::leak(prover_id.to_owned().into_boxed_str());
                PROVER_METRICS.prove_time[&label].observe(prove_time);
            }
            JobStatus::Pending => {
                tracing::warn!(block_number = block, "submitting prove for unassigned job");
            }
        };

        let program_proof = bincode_v1::deserialize(&proof).map_err(|err| {
            tracing::warn!(block, "failed to deserialize proof: {err}");
            SubmitError::DeserializationFailed(err)
        })?;

        if !verify_fri_proof(
            entry.previous_state_commitment,
            entry.commit_batch_info.clone().into(),
            program_proof,
        ) {
            tracing::warn!(
                batch_number = entry.commit_batch_info.batch_number,
                "Proof verification failed"
            );
            return Err(SubmitError::VerificationFailed);
        } else {
            tracing::info!(
                batch_number = entry.commit_batch_info.batch_number,
                "Proof was verified successfully"
            );
        }

        self.proof_storage
            .save_proof_with_prover(block, &proof, prover_id)
            .map_err(|e| SubmitError::Other(e.to_string()))?;
        drop(entry_ref);
        self.jobs.remove(&block);
        Ok(())
    }

    pub fn status(&self) -> Vec<JobState> {
        self.jobs
            .iter()
            .sorted_by_key(|entry| *entry.key())
            .map(|entry| {
                let block = *entry.key();
                match &entry.status {
                    JobStatus::Pending => JobState {
                        block_number: block,
                        status: "Pending",
                        prover_id: None,
                        assigned_seconds_ago: None,
                    },
                    JobStatus::Assigned { assigned_at } => JobState {
                        block_number: block,
                        status: "Assigned",
                        prover_id: None,
                        assigned_seconds_ago: Some((*assigned_at).elapsed().as_secs() as u32),
                    },
                }
            })
            .collect()
    }

    // The following delegate to persistent storage (stubs for now).
    pub fn available_proofs(&self) -> Vec<(u64, Vec<String>)> {
        self.proof_storage
            .get_blocks_with_proof()
            .into_iter()
            .map(|number| (number, vec!["fri".to_string()]))
            .collect()
    }

    pub fn get_fri_proof(&self, block: u64) -> Option<Vec<u8>> {
        self.proof_storage.get_proof(block)
    }
}

fn verify_fri_proof(
    previous_state_commitment: B256,
    stored_batch_info: StoredBatchInfo,
    proof: ProgramProof,
) -> bool {
    let expected_pi = BatchPublicInput {
        state_before: previous_state_commitment.0.into(),
        state_after: stored_batch_info.state_commitment.0.into(),
        batch_output: stored_batch_info.commitment.0.into(),
    };

    let expected_hash_u32s: [u32; 8] = batch_output_hash_as_register_values(&expected_pi);

    let proof_final_register_values: [u32; 16] = extract_final_register_values(proof);

    tracing::debug!(
        batch_number = stored_batch_info.batch_number,
        "Program final registers: {:?}",
        proof_final_register_values
    );
    tracing::debug!(
        batch_number = stored_batch_info.batch_number,
        "Expected values for Public Inputs hash: {:?}",
        expected_hash_u32s
    );

    // compare expected_hash_u32s with the last 8 values of proof_final_register_values
    proof_final_register_values[..8] == expected_hash_u32s
}

fn batch_output_hash_as_register_values(public_input: &BatchPublicInput) -> [u32; 8] {
    public_input
        .hash()
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("Slice with incorrect length")))
        .collect::<Vec<u32>>()
        .try_into()
        .expect("Hash should be exactly 32 bytes long")
}

fn extract_final_register_values(input_program_proof: ProgramProof) -> [u32; 16] {
    let (metadata, proof_list) = proof_list_and_metadata_from_program_proof(input_program_proof);

    let oracle_data = generate_oracle_data_from_metadata_and_proof_list(&metadata, &proof_list);
    tracing::debug!(
        "Oracle data iterator created with {} items",
        oracle_data.len()
    );

    let it = oracle_data.into_iter();

    verifier_common::prover::nd_source_std::set_iterator(it);

    // Assume that program proof has only recursion proofs.
    tracing::debug!("Running continue recursive");
    assert!(metadata.reduced_proof_count > 0);

    let final_register_values = full_statement_verifier::verify_recursion_layer();

    assert!(
        verifier_common::prover::nd_source_std::try_read_word().is_none(),
        "Expected that all words from CSR were consumed"
    );
    final_register_values
}
