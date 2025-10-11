//! RocksDB-backed persistence for Batch metadata and FRI proofs.
//! May be extracted to a separate service later on (aka FRI Cache)
//! Currently used as a general batch storage:
//!  * batch -> block mapping
//!  * block -> batch mapping (temporary using bin-search)
//!  * batch -> its FRI proof
//!  * batch -> its commitment (used for l1 senders)

use crate::prover_api::batch_index::BatchIndex;
use alloy::primitives::BlockNumber;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_object_store::_reexports::BoxedError;
use zksync_os_object_store::{Bucket, ObjectStore, ObjectStoreError, StoredObject};
use zksync_os_storage_api::{ReadBatch, ReadFinality, UpdateBatchIndex};

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
    batch_index: BatchIndex,
}

impl ProofStorage {
    pub fn new(object_store: Arc<dyn ObjectStore>, batch_index: BatchIndex) -> Self {
        Self {
            object_store,
            batch_index,
        }
    }

    /// Persist a BatchWithProof. Overwrites any existing entry for the same batch.
    /// Doesn't allow gaps - if a proof for batch `n` is missing, then no proof for batch `n+1` is allowed.
    pub async fn save_proof(&self, value: &StoredBatch) -> anyhow::Result<()> {
        self.object_store.put(value.batch_number(), value).await?;

        let (batch_number, first_block, last_block) = match value {
            StoredBatch::V1(envelope) => (
                envelope.batch_number(),
                envelope.batch.first_block_number,
                envelope.batch.last_block_number,
            ),
        };

        // This function is only used in sequencer, so most likely this insert won't require
        // any reads the bucket.
        self.write_to_index(batch_number, first_block, last_block)
            .await?;

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

    pub async fn write_to_index(
        &self,
        batch_number: u64,
        first_block: u64,
        last_block: u64,
    ) -> anyhow::Result<()> {
        if batch_number == self.batch_index.get_last_stored_batch() + 1 {
            self.batch_index
                .write_batch_mapping(batch_number, first_block, last_block)?;

            return Ok(());
        }

        let self_clone = self.clone();
        tokio::spawn(async move {
            // there is a gap - we need to fill it by scanning the object store for missing batches
            let last_indexed = self_clone.batch_index.get_last_stored_batch();
            for n in (last_indexed + 1)..=batch_number {
                // it might be already stored in case of concurrent calls, check it for optimization
                // (only possible in EN)
                if self_clone.batch_index.get_batch_range(n).is_some() {
                    continue;
                }

                let Some(envelope) = self_clone.get(n).await? else {
                    anyhow::bail!(
                        "Batch {n} not found in object store while syncing index up to {batch_number}"
                    );
                };

                self_clone.batch_index.write_batch_mapping(
                    n,
                    envelope.batch.first_block_number,
                    envelope.batch.last_block_number,
                )?;
            }

            Ok(())
        });

        Ok(())
    }
}

#[async_trait::async_trait]
impl ReadBatch for ProofStorage {
    async fn get_batch_by_block_number(
        &self,
        block_number: BlockNumber,
        _: &dyn ReadFinality,
    ) -> anyhow::Result<Option<u64>> {
        // Handle genesis block with a special case as we don't store it in the DB.
        if block_number == 0 {
            return Ok(Some(0));
        }

        Ok(self.batch_index.get_batch_by_block(block_number))
    }

    async fn get_batch_range_by_number(
        &self,
        batch_number: u64,
    ) -> anyhow::Result<Option<(BlockNumber, BlockNumber)>> {
        // Handle genesis block with a special case as we don't store it in the DB.
        if batch_number == 0 {
            return Ok(Some((0, 0)));
        }

        if let Some((first, last)) = self.batch_index.get_batch_range(batch_number) {
            return Ok(Some((first, last)));
        }

        // If the batch is not found in the index, try to load it from the object store.
        // This is a fallback for the case when the index is not fully synced.
        Ok(self.get(batch_number).await?.map(|envelope| {
            (
                envelope.batch.first_block_number,
                envelope.batch.last_block_number,
            )
        }))
    }
}

#[async_trait::async_trait]
impl UpdateBatchIndex for ProofStorage {
    async fn sync_index_to_batch(
        &self,
        batch_number: u64,
        first_block: u64,
        last_block: u64,
    ) -> anyhow::Result<()> {
        self.write_to_index(batch_number, first_block, last_block)
            .await
    }
}
