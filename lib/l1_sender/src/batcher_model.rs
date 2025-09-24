use crate::batcher_metrics::{BATCHER_METRICS, BatchExecutionStage};
use crate::commitment::{CommitBatchInfo, StoredBatchInfo};
use alloy::primitives::B256;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::time::SystemTime;
use time::UtcDateTime;
use zksync_os_observability::LatencyDistributionTracker;
// todo: these models are used throughout the batcher subsystem - not only l1 sender
//       we will move them to `types` or `batcher_types` when an analogous crate is created in `zksync-os`

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
    #[serde(default = "default_execution_version")]
    pub execution_version: u32,
}

fn default_execution_version() -> u32 {
    1
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BatchEnvelope<E> {
    pub batch: BatchMetadata,
    pub data: E,
    #[serde(skip, default)]
    pub latency_tracker: LatencyDistributionTracker<BatchExecutionStage>,
}

impl<E> BatchEnvelope<E> {
    pub fn new(batch: BatchMetadata, data: E) -> Self {
        Self {
            batch,
            data,
            latency_tracker: LatencyDistributionTracker::default(),
        }
    }
    pub fn batch_number(&self) -> u64 {
        self.batch.commit_batch_info.batch_number
    }
    pub fn time_since_first_block(&self) -> anyhow::Result<core::time::Duration> {
        let first_block_time = SystemTime::from(UtcDateTime::from_unix_timestamp(
            self.batch.commit_batch_info.first_block_timestamp as i64,
        )?);

        Ok(SystemTime::now().duration_since(first_block_time)?)
    }

    // not 100% happy with this - `BatchEnvelope` shouldn't depend on metrics
    // maybe we can put metrics logic inside `LatencyDistributionTracker` generically,
    // but then it needs to have the batch_number as its field - which makes it non-generic.
    // On the other hand, we can treat the `BatchEnvelop` model as metrics/tracking-related
    //
    // Will be revisited on next `BatchEnvelope` iteration -
    // along with the fact that we almost always only use `BatchEnvelope<FriProof>`, so it being generic may be not justified

    pub fn set_stage(&mut self, stage: BatchExecutionStage) {
        let batch_number = self.batch_number();
        let last_block_number = self.batch.last_block_number;
        self.latency_tracker.record_stage(stage, |duration| {
            BATCHER_METRICS.execution_stages[&stage].observe(duration);
            BATCHER_METRICS.batch_number[&stage].set(batch_number);
            BATCHER_METRICS.block_number[&stage].set(last_block_number);
        });
    }

    pub fn with_stage(mut self, stage: BatchExecutionStage) -> BatchEnvelope<E> {
        self.set_stage(stage);
        self
    }

    pub fn with_data<N>(self, data: N) -> BatchEnvelope<N> {
        BatchEnvelope::<N> {
            batch: self.batch,
            data,
            latency_tracker: self.latency_tracker,
        }
    }
}

pub type ProverInput = Vec<u32>;

#[derive(Clone, Serialize, Deserialize)]
pub enum FriProof {
    // Fake proof for testing purposes
    Fake,
    Real(RealFriProof),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RealFriProof {
    pub proof: Vec<u8>,
    pub snark_vk: B256,
}

impl FriProof {
    pub fn is_fake(&self) -> bool {
        matches!(self, FriProof::Fake)
    }

    pub fn snark_vk(&self) -> Option<B256> {
        match self {
            FriProof::Fake => None,
            FriProof::Real(proof) => Some(proof.snark_vk),
        }
    }
}

impl Debug for FriProof {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            FriProof::Fake => write!(f, "Fake"),
            FriProof::Real(proof) => write!(
                f,
                "Real(vk={:?}, len: {:?})",
                proof.snark_vk,
                proof.proof.len()
            ),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SnarkProof {
    // Fake proof for testing purposes
    Fake,
    Real(RealSnarkProof),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealSnarkProof {
    pub proof: Vec<u8>,
    pub vk: B256,
}

impl SnarkProof {
    pub fn vk(&self) -> Option<B256> {
        match self {
            SnarkProof::Fake => None,
            SnarkProof::Real(proof) => Some(proof.vk),
        }
    }
}
