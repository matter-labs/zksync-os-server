use alloy::primitives::B256;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::ops::RangeInclusive;
use tokio::sync::watch;
use zksync_os_merkle_tree_api::TreeBatchOutput;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::BlockOutput;

pub const DEFAULT_MAX_CACHED_TREE_BLOCKS: usize = 4096;

/// Local data needed to reconstruct batch commitments from replayed blocks.
#[derive(Clone, Debug)]
pub struct LocalBatchBlockData {
    pub output: BlockOutput,
    pub record: ReplayRecord,
    pub tree_output: TreeBatchOutput,
    pub multichain_root: B256,
}

/// Ordered cache of per-block data keyed by block number.
///
/// Blocks must be inserted consecutively (each block number exactly one greater than the last).
/// Eviction is the caller's responsibility; they decide when to call
/// [`TreeBlockCache::remove_lower_or_equal_than`]
#[derive(Debug)]
pub struct TreeBlockCache {
    data: VecDeque<LocalBatchBlockData>,
    first_block: Option<u64>,
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
            data: VecDeque::new(),
            first_block: None,
            max_blocks,
        }
    }

    /// Insert a block into the cache. Blocks must arrive consecutively (each block number exactly
    /// one greater than the last).
    pub fn insert(&mut self, block_number: u64, block: LocalBatchBlockData) -> anyhow::Result<()> {
        if let Some((_, last_block)) = self.range() {
            if block_number != last_block + 1 {
                anyhow::bail!("Out of order block received. This should never happen");
            }
        } else {
            self.first_block = Some(block_number);
        }
        self.data.push_back(block);
        Ok(())
    }

    /// Whether the cache is still below its soft bound.
    ///
    /// The bound is *soft*: the sole writer (the L1 consistency checker) is allowed to
    /// overshoot it to finish caching the blocks an oversized pending batch needs. See
    /// [`L1ConsistencyChecker::can_accept_tree_block`](crate::checker::L1ConsistencyChecker).
    pub fn has_capacity(&self) -> bool {
        self.data.len() < self.max_blocks
    }

    /// Currently cached block-number range (inclusive bounds), or `None` if empty.
    pub fn range(&self) -> Option<(u64, u64)> {
        if self.data.is_empty() {
            None
        } else {
            self.first_block
                .map(|block| (block, block + self.data.len() as u64 - 1))
        }
    }

    /// Removes all blocks lower than or equal to the given block number.
    pub fn remove_lower_or_equal_than(&mut self, block_number: u64) {
        if let Some((first_block, last_block)) = self.range() {
            if block_number < first_block {
                return;
            }

            let amount_to_remove = (block_number.min(last_block) - first_block + 1) as usize;
            self.data.drain(0..amount_to_remove);

            // Keep `first_block` aligned with the deque; an empty cache has no valid range.
            if self.data.is_empty() {
                self.first_block = None;
            } else {
                self.first_block = Some(first_block + amount_to_remove as u64);
            }
        }
    }

    /// Returns a complete cached block range, or `None` if it is not fully available yet.
    pub fn get_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Option<Vec<LocalBatchBlockData>>> {
        let Some((first_block, last_block)) = self.range() else {
            return Ok(None);
        };
        if *range.start() < first_block {
            anyhow::bail!(
                "requested local batch data range {}..={} was already evicted; cached range is {}..={}",
                range.start(),
                range.end(),
                first_block,
                last_block
            );
        }
        if *range.end() > last_block {
            return Ok(None);
        }
        let start = (*range.start() - first_block) as usize;

        Ok(Some(
            self.data
                .iter()
                .skip(start)
                .take(range.count())
                .cloned()
                .collect(),
        ))
    }
}

#[async_trait]
pub trait TreeBlockCacheReceiverExt {
    /// Waits until a complete block range is available in the cache.
    async fn wait_for_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Vec<LocalBatchBlockData>>;
}

#[async_trait]
impl TreeBlockCacheReceiverExt for watch::Receiver<TreeBlockCache> {
    async fn wait_for_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Vec<LocalBatchBlockData>> {
        let mut cache_rx = self.clone();
        loop {
            {
                // if block range is available already - return it. If not - wait until the cache is updated and check again
                if let Some(blocks) = cache_rx.borrow_and_update().get_range(range.clone())? {
                    return Ok(blocks);
                }
            }
            cache_rx.changed().await?;
        }
    }
}
