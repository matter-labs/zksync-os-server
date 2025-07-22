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
use dashmap::DashMap;
use itertools::Itertools;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::Receiver;
use zksync_os_l1_sender::commitment::CommitBatchInfo;
use crate::prover_api::prover_server::{FriJobState, ProverJobManagerState};
// ───────────── Error types ─────────────

#[derive(Error, Debug)]
pub enum SubmitError {
    #[error("proof did not pass verification")]
    VerificationFailed,
    #[error("block {0} is not known to the server")]
    UnknownJob(u64),
    #[error("internal error: {0}")]
    Other(String),
}

// ───────────── Internal state ─────────────

#[derive(Clone)]
struct JobEntry {
    prover_input: Vec<u32>,
    commit_batch_info: CommitBatchInfo,
    status: JobStatus,
}

#[derive(Clone)]
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
                continue
            }

            self.jobs.insert(
                job.block_number,
                JobEntry {
                    prover_input: job.prover_input,
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
        let entry = self
            .jobs
            .remove(&block)
            .ok_or(SubmitError::UnknownJob(block))?
            .1;

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

        if !verify_fri_proof_stub(&entry.commit_batch_info, &proof) {
            return Err(SubmitError::VerificationFailed);
        }

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
        let start_block = self.proof_storage
            .next_expected_snark_range_start();

        let gapless_proofs: Vec<Vec<u8>> = std::iter::successors(Some(start_block), |b| Some(b + 1))
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
        let fri_jobs = self.jobs
            .iter()
            .sorted_by_key(|entry| *entry.key())
            .map(|entry| {
                let block = *entry.key();
                match &entry.status {
                    JobStatus::Pending => FriJobState {
                        block_number: block,
                        status: "Pending",
                        prover_id: None,
                        assigned_seconds_ago: None,
                    },
                    JobStatus::Assigned { assigned_at } => FriJobState {
                        block_number: block,
                        status: "Assigned",
                        prover_id: None,
                        assigned_seconds_ago: Some((*assigned_at).elapsed().as_secs() as u32),
                    },
                }
            })
            .collect();
        let next_snark_job = self.get_next_snark_job().map(|(from, to, _)| (from, to));
        ProverJobManagerState {
            fri_jobs,
            next_snark_job,
            fri_proofs_count_estimate: self.proof_storage.estimate_number_of_fri_proofs(),
            snark_proofs_count_estimated: self.proof_storage.estimate_number_of_snark_proofs()
        }
    }

    pub fn get_fri_proof(&self, block: u64) -> Option<Vec<u8>> {
        self.proof_storage.get_fri_proof(block)
    }
}

// ─────────────── Stubs ───────────────

fn verify_fri_proof_stub(_info: &CommitBatchInfo, _proof: &[u8]) -> bool {
    true
}
