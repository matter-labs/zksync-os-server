use std::collections::HashMap;
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_interface::types::BlockOutput;
use zksync_os_storage_api::ReadFinality;
use zksync_os_storage_api::ReplayRecord;

use super::metrics::BATCH_VERIFICATION_CLIENT_METRICS;

/// Cache of blocks that are to be used for batch verification
/// Accepts blocks only in ascending order. Old blocks are evicted when not
/// needed through a call to remove_lower_then.
///
/// This may be optimized by using a ring buffer for data storage instead.
pub(super) struct BlockCache<Finality> {
    data: HashMap<u64, (BlockOutput, ReplayRecord, BlockMerkleTreeData)>,
    range: Option<(u64, u64)>,
    finality: Finality,
}

impl<Finality: ReadFinality> BlockCache<Finality> {
    pub fn new(finality: Finality) -> Self {
        Self {
            data: HashMap::new(),
            range: None,
            finality,
        }
    }

    /// Insert a block into the cache. Expected blocks to be added in order.
    pub fn insert(
        &mut self,
        block_number: u64,
        block: (BlockOutput, ReplayRecord, BlockMerkleTreeData),
    ) -> anyhow::Result<()> {
        self.data.insert(block_number, block);
        if let Some((low, high)) = self.range {
            if block_number != high + 1 {
                anyhow::bail!("Out of order block received. This should never happen");
            }
            self.range = Some((low, block_number));
        } else {
            self.range = Some((block_number, block_number));
        }

        // evict block for committed batches
        self.remove_lower_then(self.finality.get_finality_status().last_committed_block + 1);
        // BATCH_VERIFICATION_CLIENT_METRICS.block_cache_size updated in remove_lower_then
        Ok(())
    }

    pub fn get(
        &self,
        block_number: u64,
    ) -> Option<&(BlockOutput, ReplayRecord, BlockMerkleTreeData)> {
        self.data.get(&block_number)
    }

    /// Removes all blocks lower than the given block number
    pub fn remove_lower_then(&mut self, block_number: u64) {
        if let Some((low, _)) = self.range {
            for num in low..block_number {
                self.data.remove(&num);
            }
        }
        BATCH_VERIFICATION_CLIENT_METRICS
            .block_cache_size
            .set(self.data.len());
    }
}
