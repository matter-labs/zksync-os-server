use alloy::primitives::B256;
use std::collections::VecDeque;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use zksync_os_merkle_tree_api::TreeBatchOutput;
use zksync_os_storage_api::{ReplayRecord, TreeBlock};
use zksync_os_types::BlockOutput;

/// Ordered cache of per-block data keyed by block number.
///
/// Blocks must be inserted in strictly ascending order. Eviction is the
/// caller's responsibility; they decide when to call [`TreeBlockCache::remove_lower_then`]
#[derive(Debug)]
pub struct TreeBlockCache<Data> {
    data: VecDeque<Data>,
    first_block: Option<u64>,
}

impl<Data> TreeBlockCache<Data> {
    pub fn new() -> Self {
        Self {
            data: VecDeque::new(),
            first_block: None,
        }
    }

    /// Insert a block into the cache. Blocks must arrive in strictly ascending order.
    pub fn insert(&mut self, block_number: u64, block: Data) -> anyhow::Result<()> {
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

    pub fn get(&self, block_number: u64) -> Option<&Data> {
        if let Some((first_block, last_block)) = self.range()
            && first_block <= block_number
            && block_number <= last_block
        {
            return self.data.get((block_number - first_block) as usize);
        }
        None
    }

    /// Currently cached block-number range (inclusive bounds), or `None` if empty.
    pub fn range(&self) -> Option<(u64, u64)> {
        self.first_block
            .map(|block| (block, block + self.data.len() as u64 - 1))
    }

    /// Removes all blocks lower than the given block number.
    pub fn remove_lower_then(&mut self, block_number: u64) {
        if let Some((first_block, last_block)) = self.range() {
            if block_number <= first_block {
                return;
            }

            for _ in first_block..=(block_number - 1).min(last_block) {
                self.data.pop_front();
            }

            if !self.data.is_empty() && first_block <= block_number {
                self.first_block = Some(block_number);
            }
        }
    }
}

impl<Data> Default for TreeBlockCache<Data> {
    fn default() -> Self {
        Self::new()
    }
}

/// Local data needed to reconstruct batch commitments from replayed blocks.
#[derive(Clone, Debug)]
pub struct LocalBatchBlockData {
    pub output: BlockOutput,
    pub record: ReplayRecord,
    pub tree_output: TreeBatchOutput,
    pub multichain_root: B256,
}

#[derive(Debug, Default)]
struct LocalBatchDataCacheInner {
    cache: TreeBlockCache<LocalBatchBlockData>,
}

/// Shared cache used by EN batch verification and L1 batch persistence.
#[derive(Clone, Debug, Default)]
pub struct LocalBatchDataCache {
    inner: Arc<Mutex<LocalBatchDataCacheInner>>,
    notify: Arc<Notify>,
}

impl LocalBatchDataCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, tree_block: TreeBlock, multichain_root: B256) -> anyhow::Result<()> {
        let block_number = tree_block.record.block_context.block_number;
        let data = LocalBatchBlockData {
            output: tree_block.output,
            record: tree_block.record,
            tree_output: tree_block.tree.output,
            multichain_root,
        };

        self.inner
            .lock()
            .expect("local batch data cache mutex poisoned")
            .cache
            .insert(block_number, data)?;
        self.notify.notify_waiters();
        Ok(())
    }

    pub fn range(&self) -> Option<(u64, u64)> {
        self.inner
            .lock()
            .expect("local batch data cache mutex poisoned")
            .cache
            .range()
    }

    /// NOTE: this method is called by user and should be called after careful consideration to not clean up
    /// the cache that can still be used
    pub fn remove_lower_than(&self, block_number: u64) {
        self.inner
            .lock()
            .expect("local batch data cache mutex poisoned")
            .cache
            .remove_lower_then(block_number);
    }

    pub fn get_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Option<Vec<LocalBatchBlockData>>> {
        let start = *range.start();
        let end = *range.end();
        let guard = self
            .inner
            .lock()
            .expect("local batch data cache mutex poisoned");

        let Some((first_block, last_block)) = guard.cache.range() else {
            return Ok(None);
        };
        if start < first_block {
            anyhow::bail!(
                "requested local batch data range {start}..={end} was already evicted; cached range is {first_block}..={last_block}"
            );
        }
        if end > last_block {
            return Ok(None);
        }

        let mut result = Vec::with_capacity((end - start + 1) as usize);
        for block_number in start..=end {
            let Some(block) = guard.cache.get(block_number) else {
                anyhow::bail!(
                    "missing local batch data for block {block_number} inside cached range {first_block}..={last_block}"
                );
            };
            result.push(block.clone());
        }
        Ok(Some(result))
    }

    pub async fn wait_for_range(
        &self,
        range: RangeInclusive<u64>,
    ) -> anyhow::Result<Vec<LocalBatchBlockData>> {
        loop {
            let notified = self.notify.notified();
            if let Some(blocks) = self.get_range(range.clone())? {
                return Ok(blocks);
            }
            notified.await;
        }
    }
}
