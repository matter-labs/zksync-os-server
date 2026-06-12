use async_trait::async_trait;
use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use tokio::sync::watch;
use zksync_os_batch_types::BlockCommitmentData;

pub const DEFAULT_MAX_CACHED_TREE_BLOCKS: usize = 4096;

/// Cache of per-block commitment data.
///
/// Inserts are sequential, while concurrent verification can evict completed ranges out of order.
#[derive(Debug)]
pub struct TreeBlockCache {
    data: BTreeMap<u64, BlockCommitmentData>,
    /// Next block expected on insert; also marks ranges that are not cached yet.
    next_expected_block: Option<u64>,
    max_blocks: usize,
}

impl Default for TreeBlockCache {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeBlockCache {
    pub fn new() -> Self {
        Self::with_max_blocks(DEFAULT_MAX_CACHED_TREE_BLOCKS)
    }

    pub fn with_max_blocks(max_blocks: usize) -> Self {
        Self {
            data: BTreeMap::new(),
            next_expected_block: None,
            max_blocks,
        }
    }

    /// Inserts the next sequential block.
    pub fn insert(&mut self, block_number: u64, block: BlockCommitmentData) -> anyhow::Result<()> {
        if let Some(expected) = self.next_expected_block
            && block_number != expected
        {
            anyhow::bail!("Out of order block received. This should never happen");
        }
        self.data.insert(block_number, block);
        self.next_expected_block = Some(block_number + 1);
        Ok(())
    }

    /// Whether the cache is still below its soft bound.
    pub fn has_capacity(&self) -> bool {
        self.data.len() < self.max_blocks
    }

    /// Removes every block in the given inclusive range.
    pub fn remove_range(&mut self, range: RangeInclusive<u64>) {
        // Drop [start, end] without scanning the whole map.
        let mut at_or_above_start = self.data.split_off(range.start());
        let above_end = at_or_above_start.split_off(&(range.end() + 1));
        self.data.extend(above_end);
    }

    /// Returns a complete cached block range, or `None` if it is not fully available yet.
    pub fn get_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Option<Vec<BlockCommitmentData>>> {
        let Some(next_expected_block) = self.next_expected_block else {
            return Ok(None);
        };
        if *range.end() >= next_expected_block {
            return Ok(None);
        }

        let mut blocks = Vec::with_capacity(range.clone().count());
        for block_number in range {
            let Some(block) = self.data.get(&block_number) else {
                anyhow::bail!(
                    "requested local batch data block {block_number} was already evicted"
                );
            };
            blocks.push(block.clone());
        }
        Ok(Some(blocks))
    }
}

#[async_trait]
pub trait TreeBlockCacheReceiverExt {
    /// Waits until a complete block range is available in the cache.
    async fn wait_for_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Vec<BlockCommitmentData>>;
}

#[async_trait]
impl TreeBlockCacheReceiverExt for watch::Receiver<TreeBlockCache> {
    async fn wait_for_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Vec<BlockCommitmentData>> {
        let mut cache_rx = self.clone();
        loop {
            {
                if let Some(blocks) = cache_rx.borrow_and_update().get_range(range.clone())? {
                    return Ok(blocks);
                }
            }
            cache_rx.changed().await?;
        }
    }
}
