use alloy::primitives::BlockNumber;
use zksync_os_rocksdb::rocksdb;

pub trait ReadBatch: Send + Sync + 'static {
    /// Get the batch number that contains the given block.
    fn get_batch_by_block_number(&self, block_number: BlockNumber) -> ReadBatchResult<Option<u64>>;

    // todo: return `BatchMetadata` once it is moved to `types` crate
    /// Get batch's range (start block number and end block number) by the batch's number.
    fn get_batch_range_by_number(
        &self,
        batch_number: u64,
    ) -> ReadBatchResult<Option<(BlockNumber, BlockNumber)>>;
}

/// Batch storage result type.
pub type ReadBatchResult<Ok> = Result<Ok, ReadBatchError>;

/// Error variants thrown by batch storage.
#[derive(Debug, thiserror::Error)]
pub enum ReadBatchError {
    #[error(transparent)]
    Rocksdb(#[from] rocksdb::Error),
}
