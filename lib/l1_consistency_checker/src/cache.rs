use async_trait::async_trait;
use std::{collections::BTreeMap, mem, ops::RangeInclusive, sync::Arc};
use tokio::sync::watch;
use zksync_os_batch_types::BlockCommitmentData;

pub const DEFAULT_MAX_CACHED_TREE_BLOCK_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug)]
struct CachedBlockCommitmentData {
    block: Arc<BlockCommitmentData>,
    retained_bytes: usize,
}

impl CachedBlockCommitmentData {
    fn new(block_number: u64, block: BlockCommitmentData) -> Self {
        let retained_bytes =
            mem::size_of_val(&block_number) + mem::size_of::<usize>() + block.retained_size_bytes();
        Self {
            block: Arc::new(block),
            retained_bytes,
        }
    }
}

/// Cache of per-block commitment data.
///
/// Inserts are sequential, while concurrent verification can evict completed ranges out of order.
#[derive(Debug)]
pub struct TreeBlockCache {
    data: BTreeMap<u64, CachedBlockCommitmentData>,
    /// Next block expected on insert; also marks ranges that are not cached yet.
    next_expected_block: Option<u64>,
    cached_bytes: usize,
    max_cached_bytes: usize,
}

impl Default for TreeBlockCache {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeBlockCache {
    pub fn new() -> Self {
        Self::with_max_cached_bytes(DEFAULT_MAX_CACHED_TREE_BLOCK_BYTES)
    }

    pub fn with_max_cached_bytes(max_cached_bytes: usize) -> Self {
        Self {
            data: BTreeMap::new(),
            next_expected_block: None,
            cached_bytes: 0,
            max_cached_bytes,
        }
    }

    /// Inserts the next sequential block.
    pub fn insert(&mut self, block_number: u64, block: BlockCommitmentData) -> anyhow::Result<()> {
        if let Some(expected) = self.next_expected_block
            && block_number != expected
        {
            anyhow::bail!("Out of order block received. This should never happen");
        }
        let cached_block = CachedBlockCommitmentData::new(block_number, block);
        let retained_bytes = cached_block.retained_bytes;
        if let Some(replaced) = self.data.insert(block_number, cached_block) {
            self.cached_bytes = self
                .cached_bytes
                .checked_sub(replaced.retained_bytes)
                .expect("cached bytes underflow while replacing block");
        }
        self.cached_bytes += retained_bytes;
        self.next_expected_block = Some(block_number + 1);
        Ok(())
    }

    /// Whether the cache is still below its soft bound.
    pub fn has_capacity(&self) -> bool {
        self.data.is_empty() || self.cached_bytes < self.max_cached_bytes
    }

    /// Removes every block in the given inclusive range.
    pub fn remove_range(&mut self, range: RangeInclusive<u64>) {
        // Drop [start, end] without scanning the whole map.
        let mut at_or_above_start = self.data.split_off(range.start());
        let above_end = at_or_above_start.split_off(&(range.end() + 1));
        let removed_bytes = at_or_above_start
            .values()
            .map(|block| block.retained_bytes)
            .sum::<usize>();
        self.cached_bytes = self
            .cached_bytes
            .checked_sub(removed_bytes)
            .expect("cached bytes underflow while removing blocks");
        self.data.extend(above_end);
    }

    /// Returns a complete cached block range, or `None` if it is not fully available yet.
    pub fn get_range(
        &self,
        range: &RangeInclusive<u64>,
    ) -> anyhow::Result<Option<Vec<Arc<BlockCommitmentData>>>> {
        let Some(next_expected_block) = self.next_expected_block else {
            return Ok(None);
        };
        if *range.end() >= next_expected_block {
            return Ok(None);
        }

        let mut blocks = Vec::with_capacity(range.clone().count());
        for block_number in range.clone() {
            let Some(block) = self.data.get(&block_number) else {
                anyhow::bail!(
                    "requested local batch data block {block_number} was already evicted"
                );
            };
            blocks.push(Arc::clone(&block.block));
        }
        Ok(Some(blocks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use zksync_os_types::ProtocolSemanticVersion;

    fn block_commitment_data(block_number: u64, pubdata: Vec<u8>) -> BlockCommitmentData {
        BlockCommitmentData {
            block_number,
            timestamp: 0,
            l1_tx_onchain_hashes: Vec::new(),
            num_l2_txs: 0,
            interop_roots: Vec::new(),
            upgrade_tx_hash: None,
            encoded_l2_l1_logs: Vec::new(),
            pubdata,
            last_256_block_hashes_blake: B256::ZERO,
            tree_root_hash: B256::ZERO,
            tree_leaf_count: 0,
            multichain_root: B256::ZERO,
            protocol_version: ProtocolSemanticVersion::new(0, 31, 0),
        }
    }

    #[test]
    fn capacity_is_based_on_retained_bytes() {
        let mut cache = TreeBlockCache::with_max_cached_bytes(256);
        assert!(cache.has_capacity());

        cache
            .insert(1, block_commitment_data(1, vec![0; 1024]))
            .unwrap();
        assert!(!cache.has_capacity());

        cache.remove_range(1..=1);
        assert!(cache.has_capacity());
        assert_eq!(cache.cached_bytes, 0);
    }
}

#[async_trait]
pub trait TreeBlockCacheReceiverExt {
    /// Waits until a complete block range is available in the cache.
    async fn wait_for_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Vec<Arc<BlockCommitmentData>>>;
}

#[async_trait]
impl TreeBlockCacheReceiverExt for watch::Receiver<TreeBlockCache> {
    async fn wait_for_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Vec<Arc<BlockCommitmentData>>> {
        let mut cache_rx = self.clone();
        loop {
            {
                if let Some(blocks) = cache_rx.borrow_and_update().get_range(&range)? {
                    return Ok(blocks);
                }
            }
            cache_rx.changed().await?;
        }
    }
}
