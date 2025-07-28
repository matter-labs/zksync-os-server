use jsonrpsee::core::Serialize;
use serde::Deserialize;
use std::time::SystemTime;
use tokio::time::Instant;
use zksync_os_l1_sender::commitment::{CommitBatchInfo, StoredBatchInfo};

/// Information about a batch that is enough for all L1 operations.
/// Used throughout the batcher subsystem
/// We may want to rework it -
///    instead of putting computed CommitBatchInfo/StoredBatchInfo here (L1 contract-specific classes),
///    we may want to include lower-level fields
///
///  Note that any change to this struct is breaking since we serialize it in `ProofStorage`
///
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchMetadata {
    pub previous_stored_batch_info: StoredBatchInfo,
    pub commit_batch_info: CommitBatchInfo,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub tx_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchEnvelope<E> {
    pub batch: BatchMetadata,
    pub data: E,
    #[serde(skip, default)]
    pub trace: Trace,
}
impl<A> BatchEnvelope<A> {
    pub fn batch_number(&self) -> u64 {
        self.batch.commit_batch_info.batch_number
    }
}
/// Trace of the batch processing - has timestamps of each stage the batch went through
/// Do not use it for business logic
/// (although currently used to determine when to give up on waiting for a prover and use fake proof instead)
#[derive(Clone, Debug)]
pub struct Trace {
    pub start_time: SystemTime,
    pub start_instant: Instant,
    pub stages: Vec<(&'static str, Instant)>,
}

pub type ProverInput = Vec<u32>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FriProof {
    // Fake proof for testing purposes
    Fake,
    Real(Vec<u8>),
}

impl Trace {
    pub fn with_stage(mut self, stage: &'static str) -> Trace {
        self.stages.push((stage, Instant::now()));
        self
    }
    pub fn last_stage_age(&self) -> std::time::Duration {
        self.stages
            .last()
            .map(|(_, instant)| instant.elapsed())
            .unwrap_or(self.start_instant.elapsed())
    }
}
impl Default for Trace {
    fn default() -> Self {
        Self {
            start_time: SystemTime::now(),
            start_instant: Instant::now(),
            stages: vec![],
        }
    }
}
