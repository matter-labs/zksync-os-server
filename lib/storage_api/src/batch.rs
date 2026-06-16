use alloy::primitives::BlockNumber;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};
use zksync_os_batch_types::DiscoveredCommittedBatch;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedBatch {
    #[serde(flatten)]
    pub committed_batch: DiscoveredCommittedBatch,
    pub execute_sl_block_number: Option<u64>,
}

impl Deref for PersistedBatch {
    type Target = DiscoveredCommittedBatch;

    fn deref(&self) -> &Self::Target {
        &self.committed_batch
    }
}

impl DerefMut for PersistedBatch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.committed_batch
    }
}

pub trait ReadBatch: Send + Sync + 'static {
    /// Get batch that contains the given block.
    fn get_batch_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> anyhow::Result<Option<PersistedBatch>>;

    /// Get batch by the batch's number.
    fn get_batch_by_number(&self, batch_number: u64) -> anyhow::Result<Option<PersistedBatch>>;

    /// Returns the latest (greatest) batch's number.
    ///
    /// This method:
    /// * MUST be thread-safe
    /// * MUST be infallible, as batch storage is guaranteed to hold at least genesis under `0`
    /// * MUST be monotonically non-decreasing
    ///
    /// If this method returned `N`, then batch number `N` MUST be available in storage. However,
    /// batches `[0; N-1]` MAY be missing if they are a legacy batch (i.e. produced before
    /// `ReportCommittedBatchRangeZKsyncOS` event was being emitted on commit to settlement layer).
    fn latest_batch(&self) -> u64;
}

/// A write-capable counterpart of [`ReadBatch`] that allows to write new batches to the storage.
pub trait WriteBatch: ReadBatch {
    /// Writes a new batch to storage.
    fn write(&self, batch: PersistedBatch);
}
