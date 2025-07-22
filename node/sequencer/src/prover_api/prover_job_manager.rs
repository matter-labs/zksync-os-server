//! Concurrent in‑memory queue for FRI and SNARK prover work.
//!
//! * Incoming jobs are received through an async channel (see
//!   [`ProverJobManager::listen_for_batch_jobs`]).
//! * Temporary applies different logic to FRI and SNARK jobs:
//!   * For FRI jobs, it stores the prover assignments in the in-memory map, so that multiple provers are supported:
//!     * Provers request work via [`pick_next_fri_job`], which always returns the
//!       smallest block number that is **pending** or has **timed‑out**.
//!     * When a proof is submitted and verified, the job is removed so the map
//!       cannot grow unbounded.
//!   * For SNARK jobs, it doesn't coordinate between provers, so only one prover is
//!     supported. Additionally, it doesn't store anything in memory - using a persistent storage instead:
//!     * Provers request work via [`pick_next_snark_job`], which returns the
//!       next range of blocks that have no proofs.
//!     * When a proof is submitted, it's saved in the persistent storage.
//!
use std::time::{Duration, Instant};

use crate::model::BatchJob;
use crate::prover_api::metrics::PROVER_METRICS;
use crate::prover_api::proof_storage::ProofStorage;
use crate::prover_api::prover_server::{FriJobState, JobStatusInfo, ProverJobManagerState};
use air_compiler_cli::prover_utils::{
    generate_oracle_data_from_metadata_and_proof_list, proof_list_and_metadata_from_program_proof,
};
use alloy::primitives::B256;
use dashmap::DashMap;
use execution_utils::ProgramProof;
use itertools::Itertools;
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
// ───────────── Public API ─────────────

pub struct ProverJobManager {
    jobs: DashMap<u64, JobEntry>,
    proof_storage: ProofStorage,
    assignment_timeout: Duration,
    // applies backpressure when `jobs` reaches this number
    max_jobs_count: usize,
    max_fris_per_snark: usize,
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
        max_fris_per_snark: usize,
    ) -> Self {
        Self {
            jobs: DashMap::new(),
            proof_storage,
            assignment_timeout,
            max_jobs_count,
            max_fris_per_snark,
            pick_lock: parking_lot::Mutex::new(()),
        }
    }

    pub async fn listen_for_batch_jobs(
        self: Arc<Self>,
        mut rx: Receiver<BatchJob>,
    ) -> anyhow::Result<()> {
        while let Some(job) = rx.recv().await {
            while self.jobs.len() >= self.max_jobs_count {
                // wait for `jobs` to drop below `max_jobs_count`
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            if self.proof_storage.get_fri_proof(job.block_number).is_some() {
                // todo: during state recovery,
                //  batches that were not committed yet could have changed (regenerated) -
                //  so we shouldn't use old FRI proofs here.
                // this will be addressed when we revisit state recovery.
                tracing::debug!(block_number = job.block_number, "already proven block");
                continue;
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
    /// Adds information about this assignment to the internal state.
    pub fn pick_next_fri_job(&self) -> Option<(u64, Vec<u32>)> {
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
    pub fn submit_fri_proof(
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

        match verify_fri_proof(
            entry.previous_state_commitment,
            entry.commit_batch_info.clone().into(),
            program_proof,
        ) {
            Ok(()) => {
                tracing::info!(
                    batch_number = entry.commit_batch_info.batch_number,
                    "Proof was verified successfully"
                )
            }
            Err(err) => {
                tracing::warn!(
                    batch_number = entry.commit_batch_info.batch_number,
                    "Proof verification failed"
                );
                return Err(err);
            }
        }

        drop(entry_ref);
        self.jobs.remove(&block);

        self.proof_storage
            .save_fri_proof_with_prover(block, &proof, prover_id)
            .map_err(|e| SubmitError::Other(e.to_string()))?;
        Ok(())
    }

    /// Returns the next range of gapless FRI proofs - which can be combined into a single SNARK proof.
    /// Note that this function is not mutating the internal state, as we don't save SNARK prover job assignments.
    ///
    /// Returns (block_from, block_to, Vec<FRI proof>)
    pub fn get_next_snark_job(&self) -> Option<(u64, u64, Vec<Vec<u8>>)> {
        let start_block = self.proof_storage.next_expected_snark_range_start();

        let gapless_proofs: Vec<Vec<u8>> =
            std::iter::successors(Some(start_block), |b| Some(b + 1))
                .map(|block| self.proof_storage.get_fri_proof(block))
                // stop at the first gap
                .take_while(|opt| opt.is_some())
                .take(self.max_fris_per_snark)
                // safe after take_while
                .map(Option::unwrap)
                .collect();

        if gapless_proofs.is_empty() {
            return None; // start_block doesn't have fri proof yet
        }

        let last_block = start_block + gapless_proofs.len() as u64 - 1;
        Some((start_block, last_block, gapless_proofs))
    }
    pub fn submit_snark_proof(
        &self,
        block_number_from: u64,
        block_number_to: u64,
        proof: Vec<u8>,
    ) -> Result<(), SubmitError> {
        self.proof_storage
            .save_snark_proof(block_number_from, block_number_to, &proof)
            .map_err(|e| SubmitError::Other(e.to_string()))?;
        Ok(())
    }

    pub fn status(&self) -> ProverJobManagerState {
        let fri_jobs = self
            .jobs
            .iter()
            .sorted_by_key(|entry| *entry.key())
            .map(|entry| {
                let block = *entry.key();
                match &entry.status {
                    JobStatus::Pending => FriJobState {
                        block_number: block,
                        status: JobStatusInfo::Pending,
                    },
                    JobStatus::Assigned { assigned_at } => FriJobState {
                        block_number: block,
                        status: JobStatusInfo::Assigned {
                            seconds_ago: (*assigned_at).elapsed().as_secs() as u32,
                        },
                    },
                }
            })
            .collect();
        let next_snark_job = self.get_next_snark_job().map(|(from, to, _)| (from, to));
        ProverJobManagerState {
            fri_jobs,
            next_snark_job,
            fri_proofs_count_estimate: self.proof_storage.estimate_number_of_fri_proofs(),
            snark_proofs_count_estimated: self.proof_storage.estimate_number_of_snark_proofs(),
        }
    }

    pub fn get_fri_proof(&self, block: u64) -> Option<Vec<u8>> {
        self.proof_storage.get_fri_proof(block)
    }
}

fn verify_fri_proof(
    previous_state_commitment: B256,
    stored_batch_info: StoredBatchInfo,
    proof: ProgramProof,
) -> Result<(), SubmitError> {
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
    (proof_final_register_values[..8] == expected_hash_u32s)
        .then_some(())
        .ok_or(SubmitError::VerificationFailed)
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
