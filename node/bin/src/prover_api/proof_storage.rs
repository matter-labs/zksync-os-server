//! RocksDB-backed persistence for Batch metadata, FRI proofs and failed FRI proofs.
//! May be extracted to a separate service later on (aka FRI Cache)
//! Currently used as a general batch storage:
//!  * batch -> block mapping
//!  * block -> batch mapping (temporary using bin-search)
//!  * batch -> its FRI proof
//!  * batch -> its commitment (used for l1 senders)
//!  * batch -> failed FRI proof with batch metadata

use crate::prover_api::fri_job_manager::FailedFriProof;
use alloy::primitives::BlockNumber;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_object_store::_reexports::BoxedError;
use zksync_os_object_store::{Bucket, ObjectStore, ObjectStoreError, StoredObject};
use zksync_os_storage_api::{ReadBatch, ReadFinality};

#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StoredBatch {
    V1(BatchEnvelope<FriProof>),
}

impl StoredObject for StoredBatch {
    const BUCKET: Bucket = Bucket("fri_batch_envelopes");
    type Key<'a> = u64;

    fn encode_key(key: Self::Key<'_>) -> String {
        format!("fri_batch_envelope_{key}.json")
    }

    fn serialize(&self) -> Result<Vec<u8>, BoxedError> {
        serde_json::to_vec(self).map_err(From::from)
    }

    fn deserialize(bytes: Vec<u8>) -> Result<Self, BoxedError> {
        serde_json::from_slice(&bytes).map_err(From::from)
    }
}

impl StoredBatch {
    pub fn batch_number(&self) -> u64 {
        match self {
            StoredBatch::V1(envelope) => envelope.batch_number(),
        }
    }

    pub fn batch_envelope(self) -> BatchEnvelope<FriProof> {
        match self {
            StoredBatch::V1(envelope) => envelope,
        }
    }
}

/// Failed FRI proof stored in object store for debugging
#[derive(Debug, Serialize, Deserialize)]
pub struct StoredFailedProof {
    pub failed_proof: FailedFriProof,
}

impl StoredObject for StoredFailedProof {
    const BUCKET: Bucket = Bucket("failed_fri_proofs");
    type Key<'a> = u64;

    fn encode_key(key: Self::Key<'_>) -> String {
        format!("failed_fri_proof_{key}.json")
    }

    fn serialize(&self) -> Result<Vec<u8>, BoxedError> {
        serde_json::to_vec(self).map_err(From::from)
    }

    fn deserialize(bytes: Vec<u8>) -> Result<Self, BoxedError> {
        serde_json::from_slice(&bytes).map_err(From::from)
    }
}

impl StoredFailedProof {
    pub fn batch_number(&self) -> u64 {
        self.failed_proof.batch_metadata.batch_info.batch_number
    }
}

#[derive(Clone, Debug)]
pub struct ProofStorage {
    object_store: Arc<dyn ObjectStore>,
}

impl ProofStorage {
    pub fn new(object_store: Arc<dyn ObjectStore>) -> Self {
        Self { object_store }
    }

    /// Persist a BatchWithProof. Overwrites any existing entry for the same batch.
    /// Doesn't allow gaps - if a proof for batch `n` is missing, then no proof for batch `n+1` is allowed.
    pub async fn save_batch_with_proof(&self, value: &StoredBatch) -> anyhow::Result<()> {
        self.object_store.put(value.batch_number(), value).await?;
        Ok(())
    }

    /// Loads a BatchWithProof for `batch_number`, if present.
    pub async fn get_batch_with_proof(
        &self,
        batch_number: u64,
    ) -> anyhow::Result<Option<BatchEnvelope<FriProof>>> {
        match self.object_store.get::<StoredBatch>(batch_number).await {
            Ok(o) => Ok(Some(o.batch_envelope())),
            Err(ObjectStoreError::KeyNotFound(_)) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Save a failed FRI proof with batch metadata for debugging.
    pub async fn save_failed_proof(&self, failed_proof: &StoredFailedProof) -> anyhow::Result<()> {
        self.object_store
            .put(failed_proof.batch_number(), failed_proof)
            .await?;
        Ok(())
    }

    /// Get the failed proof for a given batch number.
    /// Returns None if no failed proof exists for this batch.
    pub async fn get_failed_proof(
        &self,
        batch_number: u64,
    ) -> anyhow::Result<Option<FailedFriProof>> {
        match self
            .object_store
            .get::<StoredFailedProof>(batch_number)
            .await
        {
            Ok(o) => Ok(Some(o.failed_proof)),
            Err(ObjectStoreError::KeyNotFound(_)) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
}

#[async_trait::async_trait]
impl ReadBatch for ProofStorage {
    async fn get_batch_by_block_number(
        &self,
        block_number: BlockNumber,
        finality: &dyn ReadFinality,
    ) -> anyhow::Result<Option<u64>> {
        // Handle genesis block with a special case as we don't store it in the DB.
        if block_number == 0 {
            return Ok(Some(0));
        }
        // todo(#259): we binsearch for the batch temporarily, to be replaced with something more efficient
        let last_committed_batch = finality.get_finality_status().last_committed_batch;
        let (mut lo, mut hi) = (1, last_committed_batch);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if let Some(batch) = self.get_batch_with_proof(mid).await? {
                if batch.batch.first_block_number > block_number {
                    hi = mid;
                } else if batch.batch.last_block_number < block_number {
                    lo = mid + 1;
                } else {
                    return Ok(Some(mid));
                }
            } else {
                // Batch with this number doesn't exist, which shouldn't happen
                tracing::warn!(
                    batch_number = mid,
                    last_committed_batch,
                    "missing from proof storage"
                );
                hi = mid;
            }
        }
        // Check that `lo` actually exists
        Ok(self.get_batch_with_proof(lo).await?.map(|_| lo))
    }

    async fn get_batch_range_by_number(
        &self,
        batch_number: u64,
    ) -> anyhow::Result<Option<(BlockNumber, BlockNumber)>> {
        // Handle genesis block with a special case as we don't store it in the DB.
        if batch_number == 0 {
            return Ok(Some((0, 0)));
        }
        Ok(self
            .get_batch_with_proof(batch_number)
            .await?
            .map(|envelope| {
                (
                    envelope.batch.first_block_number,
                    envelope.batch.last_block_number,
                )
            }))
    }
}
