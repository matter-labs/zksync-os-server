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
use dashmap::DashMap;
use itertools::Itertools;
use serde::Serialize;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::Receiver;
use zksync_os_l1_sender::commitment::CommitBatchInfo;
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
            .save_proof_with_prover(block, &proof, prover_id)
            .map_err(|e| SubmitError::Other(e.to_string()))?;
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

// ─────────────── Stubs ───────────────

fn verify_fri_proof_stub(_info: &CommitBatchInfo, _proof: &[u8]) -> bool {
    true
}
