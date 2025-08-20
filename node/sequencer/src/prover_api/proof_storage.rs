//! RocksDB-backed persistence for Batch metadata and FRI proofs.
//! May be extracted to a separate service later on (aka FRI Cache)
//!

use alloy::primitives::BlockNumber;
use std::sync::Arc;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_object_store::{ObjectStore, ObjectStoreError};
use zksync_os_storage_api::ReadBatch;

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
    pub async fn save_proof(&self, value: &BatchEnvelope<FriProof>) -> anyhow::Result<()> {
        self.object_store
            .put(value.batch.commit_batch_info.batch_number, value)
            .await?;
        Ok(())
    }

    /// Loads a BatchWithProof for `batch_number`, if present.
    pub async fn get(&self, batch_number: u64) -> anyhow::Result<Option<BatchEnvelope<FriProof>>> {
        match self.object_store.get(batch_number).await {
            Ok(o) => Ok(Some(o)),
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
