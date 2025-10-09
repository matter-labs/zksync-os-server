use crate::ReadFinality;
use alloy::primitives::BlockNumber;

#[async_trait::async_trait]
pub trait ReadBatch: Send + Sync + 'static {
    /// Get the batch number that contains the given block.
    async fn get_batch_by_block_number(
        &self,
        block_number: BlockNumber,
        // todo(#205): remove/refactor once batch storage cache is in place
        finality: &dyn ReadFinality,
    ) -> anyhow::Result<Option<u64>>;

    // todo: return `BatchMetadata` once it is moved to `types` crate
    /// Get batch's range (start block number and end block number) by the batch's number.
    async fn get_batch_range_by_number(
        &self,
        batch_number: u64,
    ) -> anyhow::Result<Option<(BlockNumber, BlockNumber)>>;
}
