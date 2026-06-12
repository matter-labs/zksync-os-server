use async_trait::async_trait;
use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use tokio::sync::watch;
use zksync_os_batch_types::BlockCommitmentData;

pub const DEFAULT_MAX_CACHED_TREE_BLOCKS: usize = 4096;

/// Cache of per-block commitment data keyed by block number.
///
/// Blocks must be inserted consecutively (each block number exactly one greater than the last).
/// Eviction is the caller's responsibility via [`TreeBlockCache::remove_range`]; because the
/// L1 consistency checker verifies batches concurrently, batches can be verified — and their
/// blocks evicted — out of order, leaving gaps in the otherwise consecutive key space. A
/// [`BTreeMap`] (rather than a `VecDeque`) lets us represent those gaps.
#[derive(Debug)]
pub struct TreeBlockCache {
    data: BTreeMap<u64, BlockCommitmentData>,
    /// Block number expected on the next [`insert`](Self::insert). Monotonically increasing and
    /// unaffected by eviction, so it doubles as the boundary between "was inserted (and possibly
    /// already evicted)" and "not cached yet" when serving range reads.
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

    /// Insert a block into the cache. Blocks must arrive consecutively (each block number exactly
    /// one greater than the last), regardless of any gaps left behind by eviction.
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
    ///
    /// The bound counts blocks actually held, so gaps left by out-of-order eviction free up
    /// capacity. It must comfortably exceed the combined block span of all batches verified
    /// concurrently, otherwise intake stalls and the upstream pipeline channel eventually
    /// overflows.
    pub fn has_capacity(&self) -> bool {
        self.data.len() < self.max_blocks
    }

    /// Removes every block in the given (inclusive) range. Used to evict a single batch's blocks
    /// once it has been verified; ranges of distinct batches never overlap.
    pub fn remove_range(&mut self, range: RangeInclusive<u64>) {
        // Split out the keys `>= start`, then split the remaining-to-keep keys `> end` back off,
        // leaving only `[start, end]` to drop. Avoids an O(n) scan of the whole map.
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
            // Nothing has ever been inserted; the blocks are simply not cached yet.
            return Ok(None);
        };
        if *range.end() >= next_expected_block {
            // The tail of the range hasn't been folded into the cache yet.
            return Ok(None);
        }

        // Every block in the range was inserted at some point (its end is below `next_expected_block`),
        // so a missing block means it was already evicted after its batch was verified.
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
                // if block range is available already - return it. If not - wait until the cache is updated and check again
                if let Some(blocks) = cache_rx.borrow_and_update().get_range(range.clone())? {
                    return Ok(blocks);
                }
            }
            cache_rx.changed().await?;
        }
    }
}
