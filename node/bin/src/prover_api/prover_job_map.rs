use crate::prover_api::fri_job_manager::{FriJob, JobState};
use dashmap::DashMap;
use itertools::{Itertools, MinMaxResult};
use std::time::{Duration, Instant};
use zksync_os_l1_sender::batcher_model::{BatchMetadata, ProverInput, SignedBatchEnvelope};
use zksync_os_multivm::proving_run_execution_version;

#[derive(Debug)]
pub struct AssignedJobEntry {
    pub batch_envelope: SignedBatchEnvelope<ProverInput>,
    pub assigned_at: Instant,
}

/// Concurrent map of jobs that are currently assigned to provers.
/// Keys are batch numbers.
#[derive(Debug)]
pub struct ProverJobMap {
    // == state ==
    jobs: DashMap<u64, AssignedJobEntry>,

    // == config ==
    // assigns to another prover if it takes longer than this
    assignment_timeout: Duration,
}

impl ProverJobMap {
    pub fn new(assignment_timeout: Duration) -> Self {
        Self {
            jobs: DashMap::new(),
            assignment_timeout,
        }
    }

    /// Inserts a job just assigned to a prover.
    /// If an entry already exists for the same batch number, it is overwritten.
    pub fn insert(&self, batch_envelope: SignedBatchEnvelope<ProverInput>) {
        let job_id = batch_envelope.batch_number();
        let job_entry = AssignedJobEntry {
            batch_envelope,
            assigned_at: Instant::now(),
        };
        self.jobs.insert(job_id, job_entry);
    }

    /// Picks the **smallest** batch number whose job has timed out, if any.
    /// Returns `None` if no job has timed‑out.
    ///
    /// Thread safety:
    ///   Races are possible if multiple threads call this at the same time.
    ///   Some calls may return `None` even if others observe a timed‑out job.
    ///   This is acceptable; callers will simply poll again.
    pub fn pick_timed_out_job(&self) -> Option<(FriJob, ProverInput)> {
        let now = Instant::now();

        // Single scan to locate the minimal eligible key.
        let candidate = self
            .jobs
            .iter()
            .filter_map(|entry| {
                if now.duration_since(entry.assigned_at) > self.assignment_timeout {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .min();

        if let Some(batch_number) = candidate
            && let Some(mut entry) = self.jobs.get_mut(&batch_number)
        {
            tracing::info!(
                batch_number,
                elapsed = ?now.duration_since(entry.assigned_at),
                "Picked a timed out FRI job"
            );
            // Refresh assignment time to avoid immediate re-pick.
            entry.assigned_at = now;
            let proving_execution_version =
                proving_run_execution_version(entry.batch_envelope.batch.execution_version);
            return Some((
                FriJob {
                    batch_number: entry.batch_envelope.batch_number(),
                    vk_hash: proving_execution_version.vk_hash().to_string(),
                },
                entry.batch_envelope.data.clone(),
            ));
        }
        None
    }

    /// If a job is present for given batch_number, returns
    /// (assigned_at, batch_metadata)
    pub fn get(&self, batch_number: u64) -> Option<(Instant, BatchMetadata)> {
        self.jobs
            .get(&batch_number)
            .map(|entry| (entry.assigned_at, entry.batch_envelope.batch.clone()))
    }

    /// If a job is present for given batch_number, returns prover_input
    pub fn get_batch_data(&self, batch_number: u64) -> Option<(&'static str, ProverInput)> {
        self.jobs.get(&batch_number).map(|entry| {
            (
                entry.batch_envelope.batch.verification_key_hash(),
                entry.batch_envelope.data.clone(),
            )
        })
    }

    /// Removes and returns the assigned job entry, if present.
    pub fn remove(&self, batch_number: u64) -> Option<AssignedJobEntry> {
        self.jobs.remove(&batch_number).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn status(&self) -> Vec<JobState> {
        self.jobs
            .iter()
            .map(|r| JobState {
                fri_job: FriJob {
                    batch_number: r.batch_envelope.batch_number(),
                    vk_hash: r.batch_envelope.batch.verification_key_hash().to_string(),
                },
                assigned_seconds_ago: r.assigned_at.elapsed().as_secs(),
            })
            .sorted_by_key(|e| e.fri_job.batch_number)
            .collect()
    }

    pub fn minmax_assigned_batch_number(&self) -> MinMaxResult<u64> {
        self.jobs
            .iter()
            .map(|r| r.batch_envelope.batch_number())
            .minmax()
    }
}
