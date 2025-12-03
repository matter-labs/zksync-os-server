use std::fmt::Debug;
use std::time::{Duration, Instant};
use zksync_os_l1_sender::batcher_model::SignedBatchEnvelope;
use zksync_os_types::ProvingVersion;

#[derive(Debug)]
pub struct JobEntry<T> {
    pub batch_envelope: SignedBatchEnvelope<T>,
    pub metadata: JobMetadata,
}

#[derive(Clone, Debug)]
pub struct JobMetadata {
    pub batch_number: u64,
    pub proving_version: ProvingVersion,
    pub tx_count: usize,
    pub added_at: Instant,
    pub assigned_to_prover_id: Option<String>,
    pub assigned_at: Option<Instant>,
    pub current_attempt: usize, // 0 = never assigned, 1+ = assigned N times
}

pub enum QueueStatistics {
    Empty,
    NonEmpty(NonEmptyQueueStatistics),
}

pub struct NonEmptyQueueStatistics {
    pub min_batch_added_at: Instant,
    pub min_batch_current_attempt: usize,
    pub min_batch_number: u64,
    pub max_batch_number: u64,
    pub jobs_count: usize,
}

impl Debug for QueueStatistics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueStatistics::Empty => write!(f, "Empty queue"),
            QueueStatistics::NonEmpty(stats) => write!(
                f,
                "Queue has {} jobs, range: {} - {}, oldest job: added {:?} ago, has {} attempts.",
                stats.jobs_count,
                stats.min_batch_number,
                stats.max_batch_number,
                stats.min_batch_added_at.elapsed(),
                stats.min_batch_current_attempt
            ),
        }
    }
}

impl JobMetadata {
    pub fn new_from_batch<T>(batch_envelope: &SignedBatchEnvelope<T>) -> Self {
        let batch_number = batch_envelope.batch_number();
        let proving_version = batch_envelope
            .batch
            .proving_version()
            .expect("Must be valid execution as set by the server");
        let tx_count = batch_envelope.batch.tx_count;

        Self {
            batch_number,
            proving_version,
            tx_count,
            added_at: Instant::now(),
            assigned_to_prover_id: None,
            assigned_at: None,
            current_attempt: 0,
        }
    }

    /// Assign (or reassign) this job to a prover.
    pub fn assign(&mut self, assigned_at: Instant, assigned_to_prover_id: String) {
        self.assigned_at = Some(assigned_at);
        self.assigned_to_prover_id = Some(assigned_to_prover_id);
        self.current_attempt += 1;
    }
}

/// Statistics about a batch of jobs for logging and metrics
/// For FRI jobs - always one batch; for SNARK - can be multiple consecutive batches
pub struct JobBatchStats {
    pub min_batch_number: u64,
    pub max_batch_number: u64,
    pub proving_version: ProvingVersion,
    pub max_time_since_added: Duration,
    pub total_txs: usize,
    // if at least one of the batches was already assigned
    pub job_with_max_attempts_info: Option<PreviousAttemptsInfo>,
}

pub(super) struct PreviousAttemptsInfo {
    pub attempts: usize,
    pub time_since_last_assignment: Duration,
    pub last_assigned_to: String,
}

impl JobBatchStats {
    pub fn new(metadata_list: &[JobMetadata]) -> Self {
        assert!(!metadata_list.is_empty());

        let min_batch = &metadata_list[0];
        let max_batch_number = metadata_list[metadata_list.len() - 1].batch_number;
        let job_with_max_attempts = metadata_list
            .iter()
            .max_by_key(|m| m.current_attempt)
            .unwrap();

        let job_with_max_attempts_info = if job_with_max_attempts.current_attempt > 0 {
            Some(PreviousAttemptsInfo {
                attempts: job_with_max_attempts.current_attempt,
                time_since_last_assignment: job_with_max_attempts.assigned_at.unwrap().elapsed(),
                last_assigned_to: job_with_max_attempts.assigned_to_prover_id.clone().unwrap(),
            })
        } else {
            None
        };

        JobBatchStats {
            min_batch_number: min_batch.batch_number,
            max_batch_number,
            proving_version: min_batch.proving_version,
            max_time_since_added: min_batch.added_at.elapsed(),
            total_txs: metadata_list.iter().map(|m| m.tx_count).sum(),
            job_with_max_attempts_info,
        }
    }
    #[allow(dead_code)]
    fn format_batch_range(batch_numbers: &[u64]) -> String {
        match batch_numbers.len() {
            0 => String::from("none"),
            1 => format!("{}", batch_numbers[0]),
            _ => format!(
                "{}-{}",
                batch_numbers[0],
                batch_numbers[batch_numbers.len() - 1]
            ),
        }
    }
}

impl Debug for JobBatchStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.min_batch_number == self.max_batch_number {
            write!(f, "Batch {}", self.min_batch_number,)?;
        } else {
            write!(
                f,
                "{} Batches ({}-{})",
                self.max_batch_number - self.min_batch_number + 1,
                self.min_batch_number,
                self.max_batch_number,
            )?;
        }
        write!(
            f,
            " with {} txs, proving version {:?}, spent in queue: {:?}",
            self.total_txs, self.proving_version, self.max_time_since_added
        )?;
        if let Some(info) = &self.job_with_max_attempts_info {
            write!(
                f,
                ", last assigned to '{}', {} attempts, {:?} since last assignment",
                info.last_assigned_to, info.attempts, info.time_since_last_assignment
            )?;
        }
        Ok(())
    }
}
