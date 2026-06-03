use crate::{
    BlockUpdates,
    metrics::{LogsCacheLabels, METRICS},
};
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{B256, BlockNumber, Bloom};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::transports::{TransportErrorKind, TransportResult};
use futures::future::BoxFuture;
use std::{collections::VecDeque, mem, sync::Arc};
use tokio::sync::{RwLock, watch};
use zksync_os_provider::NodeProvider;

const UNSYNCED_BLOCK_UPDATES: BlockUpdates = BlockUpdates {
    latest_block: BlockNumber::MAX,
    finalized_block: BlockNumber::MAX,
};

#[derive(Debug)]
struct CachedBlockLogs {
    hash: B256,
    logs: Vec<Log>,
    approx_bytes: usize,
}

impl CachedBlockLogs {
    fn new(hash: B256, logs: Vec<Log>) -> Self {
        let approx_bytes = mem::size_of::<Self>()
            + logs.capacity() * mem::size_of::<Log>()
            + logs
                .iter()
                .map(|log| mem::size_of_val(log.topics()) + log.data().data.len())
                .sum::<usize>();
        Self {
            hash,
            logs,
            approx_bytes,
        }
    }
}

/// In-memory cache for logs from recent blocks.
/// Logs from `capacity` most recent blocks are stored.
/// New blocks are added with `push_head`.
/// For reorg handling & detection `cached_hash` function is provided.
#[derive(Debug)]
struct RecentLogs {
    /// The maximum number of blocks to store in the cache.
    capacity: usize,
    /// The chain head current cache corresponds to.
    synced_with: BlockUpdates,
    first_block: Option<u64>,
    /// Logs & block hashes for blocks from `first_block` to `first_block + blocks.len() - 1`
    blocks: VecDeque<CachedBlockLogs>,
    /// Approximate retained size of cached log payloads in bytes.
    approx_bytes: usize,
}

impl RecentLogs {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            synced_with: UNSYNCED_BLOCK_UPDATES,
            first_block: None,
            blocks: VecDeque::new(),
            approx_bytes: 0,
        }
    }

    /// If the cache contains all blocks from `from_block` to `to_block`.
    /// Returns an iterator over the logs from these blocks
    /// Otherwise returns `None`
    fn cached_logs_in_range(
        &self,
        from_block: u64,
        to_block: u64,
    ) -> Option<impl Iterator<Item = &Log>> {
        if !self.contains_range(from_block, to_block) {
            return None;
        }

        let first_block = self.first_block?;
        let from_offset = (from_block - first_block) as usize;
        let to_offset = (to_block - first_block) as usize;

        Some(
            self.blocks
                .range(from_offset..=to_offset)
                .flat_map(|cached_block| cached_block.logs.iter()),
        )
    }

    /// Returns the hash of the block at depth `block_number` according to the cache, if present.
    ///
    /// Can differ from current canonical chain. This is used for reorg handling.
    fn cached_hash(&self, block_number: u64) -> Option<B256> {
        let first_block = self.first_block?;
        let offset = block_number.checked_sub(first_block)? as usize;
        self.blocks.get(offset).map(|block| block.hash)
    }

    /// Adds information about a new block to the cache.
    /// If the cache contains blocks with height at least `number` they will be discarded/reverted.
    /// Returns an error if after reverts adding this block results in non-continuous block numbers.
    ///
    /// This function does not verify that previous block's hash matches the parent_hash of the new
    /// one. This should be done by the user.
    fn push_head(&mut self, number: u64, hash: B256, logs: Vec<Log>) -> TransportResult<()> {
        if self.capacity == 0 {
            return Ok(());
        }
        while self
            .latest_block()
            .is_some_and(|latest_block| latest_block >= number)
        {
            let removed = self
                .blocks
                .pop_back()
                .expect("cache tail must exist when latest_block() is present");
            self.approx_bytes -= removed.approx_bytes;
        }
        if let Some(latest_block) = self.latest_block()
            && latest_block.checked_add(1) != Some(number)
        {
            return Err(TransportErrorKind::custom_str(
                "recent logs cache cannot append a non-contiguous block",
            ));
        }

        if self.blocks.is_empty() {
            self.first_block = Some(number);
        }
        let cached_block = CachedBlockLogs::new(hash, logs);
        self.approx_bytes += cached_block.approx_bytes;
        self.blocks.push_back(cached_block);
        while self.blocks.len() > self.capacity {
            let removed = self
                .blocks
                .pop_front()
                .expect("cache head must exist when capacity eviction runs");
            self.approx_bytes -= removed.approx_bytes;
            *self
                .first_block
                .as_mut()
                .expect("first_block must be present when cache contains blocks") += 1;
        }

        Ok(())
    }

    /// Largest `block_number` that is present in the cache.
    fn latest_block(&self) -> Option<u64> {
        Some(self.first_block? + (self.blocks.len().checked_sub(1)? as u64))
    }

    /// Check if all blocks in the range are present in the cache.
    fn contains_range(&self, from_block: u64, to_block: u64) -> bool {
        let (Some(first), Some(last)) = (self.first_block, self.latest_block()) else {
            return false;
        };
        from_block <= to_block && from_block >= first && to_block <= last
    }
}

/// This structure exposes get_logs with signature identical to provider.get_logs.
/// And should be used by watchers to get recent blocks instead of the provider. As it reduces the
/// number of RPC calls/RPC cost.
///
/// Currently, it reads all the logs for new blocks in one call.
/// And remembers them for last `watcher_config.capacity` blocks.
///
/// TODO: As of now there is no filtering for these logs. Although with current settings memory usage shouldn't be a problem.
/// TODO: In reorg checks we do additional eth_getBlockByNumber - this can be avoided by extending BlockUpdates.
#[derive(Clone, Debug)]
pub struct LogsCache {
    provider: NodeProvider,
    block_updates: watch::Receiver<BlockUpdates>,
    metric_labels: LogsCacheLabels,
    recent: Arc<RwLock<RecentLogs>>,
}

impl LogsCache {
    pub fn new(
        provider: NodeProvider,
        block_updates: watch::Receiver<BlockUpdates>,
        capacity: usize,
        chain_id: u64,
    ) -> Self {
        Self {
            provider,
            block_updates,
            metric_labels: LogsCacheLabels { chain_id },
            recent: Arc::new(RwLock::new(RecentLogs::new(capacity))),
        }
    }

    /// Identical to alloy's get_logs but with caching optimizations.
    pub async fn get_logs(&self, filter: &Filter) -> TransportResult<Vec<Log>> {
        if let Err(err) = self.synchronize_if_needed().await {
            tracing::warn!(
                ?err,
                "Recent logs cache synchronization failed; Clearing cache & not using it for this request."
            );
            let mut recent = self.recent.write().await;
            let capacity = recent.capacity;
            *recent = RecentLogs::new(capacity);
            METRICS.logs_cache_approx_memory[&self.metric_labels].set(0);
        }

        let cached_logs = if let (Some(from_block), Some(to_block)) = filter.extract_block_range() {
            self.recent
                .read()
                .await
                .cached_logs_in_range(from_block, to_block)
                .map(|logs| {
                    logs.filter(|log| filter.rpc_matches(log))
                        .cloned()
                        .collect()
                })
        } else {
            None
        };

        if let Some(cached_logs) = cached_logs {
            METRICS.logs_cache_hits[&self.metric_labels].inc();
            Ok(cached_logs)
        } else {
            METRICS.logs_cache_fallbacks[&self.metric_labels].inc();
            self.provider.get_logs(filter).await
        }
    }

    /// If the chain head has changed, check for reorgs & add new blocks.
    ///
    /// We check for reverts if either latest or latest finalized has changed.
    /// This is not exact but it keeps the behavior consistent with how this worked previously.
    async fn synchronize_if_needed(&self) -> TransportResult<()> {
        let latest_snapshot = *self.block_updates.borrow();
        if self.recent.read().await.synced_with == latest_snapshot {
            return Ok(());
        }

        let mut recent = self.recent.write().await;
        if recent.synced_with != latest_snapshot && recent.capacity > 0 {
            let target_head = latest_snapshot.latest_block;
            let floor = target_head.saturating_sub(recent.capacity as u64 - 1);
            self.update_block(&mut recent, target_head, floor).await?;
        }
        recent.synced_with = latest_snapshot;
        Ok(())
    }

    /// Recursive helper that adds new blocks to the recent logs cache & handles reorgs.
    fn update_block<'a>(
        &'a self,
        recent: &'a mut RecentLogs,
        block_number: u64,
        floor: u64,
    ) -> BoxFuture<'a, TransportResult<()>> {
        Box::pin(async move {
            if block_number < floor {
                return Ok(());
            }

            // Ensure the parent block is cached before fetching this one.
            let has_parent = block_number > floor;
            if has_parent && recent.cached_hash(block_number - 1).is_none() {
                self.update_block(recent, block_number - 1, floor).await?;
            }

            let block = self
                .provider
                .get_block_by_number(BlockNumberOrTag::Number(block_number))
                .await?
                .ok_or_else(|| TransportErrorKind::custom_str("block not found"))?;
            let logs = self
                .provider
                .get_logs(&Filter::new().at_block_hash(block.header.hash))
                .await?;
            // A very rare corner case is introduced here.
            // We get `block`. Reorg happens before we get `logs`. Some RPCs would return
            // empty list(instead of proper logs) or an error.
            if logs.is_empty() && block.header.logs_bloom != Bloom::ZERO {
                return Err(TransportErrorKind::custom_str(
                    "RPC returned empty logs, but the block has logs. Most likely due to reorg.",
                ));
            }

            // Reorg check: our cached parent hash doesn't match the block's parent_hash.
            let parent_hash_mismatch = has_parent
                && recent.cached_hash(block_number - 1) != Some(block.header.parent_hash);
            if parent_hash_mismatch {
                tracing::warn!("recent logs cache detected reorg at block {block_number}");
                // Update blocks to match current chain
                self.update_block(recent, block_number - 1, floor).await?;
                // Re-fetch this block from the start for the rare case where `block` & `logs` got
                // reorged while we were fetching the previous blocks.
                self.update_block(recent, block_number, floor).await?;
                return Ok(());
            }

            recent.push_head(block_number, block.header.hash, logs)?;
            METRICS.logs_cache_blocks_loaded[&self.metric_labels].inc();
            METRICS.logs_cache_approx_memory[&self.metric_labels].set(recent.approx_bytes);
            Ok(())
        })
    }
}
