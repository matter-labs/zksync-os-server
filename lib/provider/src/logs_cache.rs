use crate::NodeProvider;
use crate::metrics::{LogCacheLabels, METRICS};
use alloy::eips::BlockNumberOrTag;
use alloy::network::{Ethereum, Network};
use alloy::primitives::{B256, Bloom};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::transports::{TransportErrorKind, TransportResult};
use futures::future::BoxFuture;
use std::{collections::VecDeque, mem, sync::Arc};
use tokio::sync::{RwLock, watch};

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
    synced_with_hash: B256,
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
            synced_with_hash: B256::ZERO,
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
                "log cache expects continuous blocks",
            ));
        }
        let cached = CachedBlockLogs::new(hash, logs);
        self.approx_bytes += cached.approx_bytes;
        self.blocks.push_back(cached);
        self.first_block.get_or_insert(number);
        while self.blocks.len() > self.capacity {
            let removed = self
                .blocks
                .pop_front()
                .expect("cache head must exist when blocks.len() > capacity");
            self.approx_bytes -= removed.approx_bytes;
            *self
                .first_block
                .as_mut()
                .expect("first_block must exist when evicting from non-empty cache") += 1;
        }
        Ok(())
    }

    fn contains_range(&self, from_block: u64, to_block: u64) -> bool {
        let Some(first_block) = self.first_block else {
            return false;
        };
        let last_block = self
            .latest_block()
            .expect("latest_block must exist when first_block does");
        from_block >= first_block && to_block <= last_block
    }

    fn latest_block(&self) -> Option<u64> {
        self.first_block
            .zip(self.blocks.len().checked_sub(1))
            .map(|(first_block, len_minus_one)| first_block + len_minus_one as u64)
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
#[derive(Clone, Debug)]
pub struct LogsCache {
    provider: NodeProvider,
    latest_blocks: watch::Receiver<<Ethereum as Network>::HeaderResponse>,
    chain_id: u64,
    recent: Arc<RwLock<RecentLogs>>,
}

impl LogsCache {
    pub async fn new(provider: NodeProvider, capacity: usize, chain_id: u64) -> Self {
        Self {
            latest_blocks: provider.latest_header_poller().await,
            provider,
            chain_id,
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
            METRICS[&LogCacheLabels(self.chain_id)].approx_memory.set(0);
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
            METRICS[&LogCacheLabels(self.chain_id)].hits.inc();
            Ok(cached_logs)
        } else {
            METRICS[&LogCacheLabels(self.chain_id)].fallbacks.inc();
            self.provider.get_logs(filter).await
        }
    }

    /// If the chain head has changed, check for reorgs & add new blocks.
    async fn synchronize_if_needed(&self) -> TransportResult<()> {
        let latest_snapshot = self.latest_blocks.borrow().clone();

        let latest_hash = latest_snapshot.hash;
        if self.recent.read().await.synced_with_hash == latest_hash {
            return Ok(());
        }

        let mut recent = self.recent.write().await;
        if recent.synced_with_hash != latest_hash && recent.capacity > 0 {
            let target_head = latest_snapshot.number;
            let floor = target_head.saturating_sub(recent.capacity as u64 - 1);
            self.update_block(&mut recent, target_head, floor).await?;
        }
        recent.synced_with_hash = latest_hash;
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
                && recent
                    .cached_hash(block_number - 1)
                    .is_some_and(|hash| hash != block.header.parent_hash);
            if parent_hash_mismatch {
                self.update_block(recent, block_number - 1, floor).await?;
            }

            recent.push_head(block_number, block.header.hash, logs)?;
            METRICS[&LogCacheLabels(self.chain_id)].blocks_loaded.inc();
            METRICS[&LogCacheLabels(self.chain_id)]
                .approx_memory
                .set(recent.approx_bytes);
            Ok(())
        })
    }
}
