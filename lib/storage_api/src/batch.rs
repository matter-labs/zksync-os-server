use alloy::primitives::BlockNumber;
use zksync_os_batch_types::DiscoveredCommittedBatch;

pub trait ReadBatch: Send + Sync + 'static {
    /// Get batch that contains the given block.
    fn get_batch_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> anyhow::Result<Option<DiscoveredCommittedBatch>>;

    /// Get batch by the batch's number.
    fn get_batch_by_number(
        &self,
        batch_number: u64,
    ) -> anyhow::Result<Option<DiscoveredCommittedBatch>>;

    /// Returns the latest (greatest) batch's number.
    fn latest_batch(&self) -> u64;
}

/// A write-capable counterpart of [`ReadBatch`] that allows to write new batches to the storage.
pub trait WriteBatch: ReadBatch {
    /// Writes a new batch to storage.
    fn write(&self, batch: DiscoveredCommittedBatch);
}
