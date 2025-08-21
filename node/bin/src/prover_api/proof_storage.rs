//! RocksDB-backed persistence for Batch metadata and FRI proofs.
//! May be extracted to a separate service later on (aka FRI Cache)
//!

use alloy::primitives::BlockNumber;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_object_store::_reexports::BoxedError;
use zksync_os_object_store::{Bucket, ObjectStore, ObjectStoreError, StoredObject};
use zksync_os_storage_api::ReadBatch;

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
    pub async fn save_proof(&self, value: &StoredBatch) -> anyhow::Result<()> {
        self.object_store.put(value.batch_number(), value).await?;
        Ok(())
    }

    /// Loads a BatchWithProof for `batch_number`, if present.
    pub async fn get(&self, batch_number: u64) -> anyhow::Result<Option<BatchEnvelope<FriProof>>> {
        match self.object_store.get::<StoredBatch>(batch_number).await {
            Ok(o) => Ok(Some(o.batch_envelope())),
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
    ) -> anyhow::Result<Option<u64>> {
        // Handle genesis block with a special case as we don't store it in the DB.
        if block_number == 0 {
            return Ok(Some(0));
        }
        // todo: insanely inefficient, to be replaced
        for batch_number in 1..=block_number {
            if let Some(batch) = self.get(batch_number).await? {
                if batch.batch.first_block_number <= block_number
                    && batch.batch.last_block_number >= block_number
                {
                    return Ok(Some(batch_number));
                }
            } else {
                return Ok(None);
            }
        }
        Ok(None)
    }

    async fn get_batch_range_by_number(
        &self,
        batch_number: u64,
    ) -> anyhow::Result<Option<(BlockNumber, BlockNumber)>> {
        // Handle genesis block with a special case as we don't store it in the DB.
        if batch_number == 0 {
            return Ok(Some((0, 0)));
        }
        Ok(self.get(batch_number).await?.map(|envelope| {
            (
                envelope.batch.first_block_number,
                envelope.batch.last_block_number,
            )
        }))
    }
}
