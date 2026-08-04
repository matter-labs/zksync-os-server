use crate::zisk_bytes::ZiskBlockBytes;
use zksync_os_batch_types::batcher_model::ProverInput;
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_pipeline::HasBlockRangeEnd;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::BlockOutput;

/// Message flowing from `ProverInputGenerator` → `Batcher`.
pub struct ProverBlock {
    pub output: BlockOutput,
    pub record: ReplayRecord,
    pub prover_input: ProverInput,
    pub tree_output: TreeBatchOutput,
    /// Second proof-system per-block input. Node-internal side channel that
    /// keeps the ZiSK bytes off the shared `ProverInput`. Present only when the
    /// second proof system is enabled and the build succeeded; `None`
    /// otherwise.
    pub zisk_block_data: Option<ZiskBlockBytes>,
}

impl HasBlockRangeEnd for ProverBlock {
    fn block_number(&self) -> u64 {
        self.record.block_context.block_number
    }
    fn block_timestamp(&self) -> Option<u64> {
        Some(self.record.block_context.timestamp)
    }
}
