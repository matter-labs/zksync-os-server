use crate::{BlockUpdates, metrics::METRICS};
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{B256, BlockNumber};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log};
use alloy::transports::{TransportErrorKind, TransportResult};
use futures::future::BoxFuture;
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::{RwLock, watch};

const UNSYNCED_BLOCK_UPDATES: BlockUpdates = BlockUpdates {
    latest_block: BlockNumber::MAX,
    finalized_block: BlockNumber::MAX,
};

#[derive(Debug)]
struct CachedBlockLogs {
    hash: B256,
    logs: Vec<Log>,
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
}

impl RecentLogs {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            synced_with: UNSYNCED_BLOCK_UPDATES,
            first_block: None,
            blocks: VecDeque::new(),
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
            self.blocks.pop_back();
        }
        if let Some(latest_block) = self.latest_block()
            && latest_block.checked_add(1) != Some(number)
        {
            return Err(TransportErrorKind::custom_str(
                "recent logs cache cannot append a non-contiguous block",
            )
            .into());
        }

        if self.blocks.is_empty() {
            self.first_block = Some(number);
        }
        self.blocks.push_back(CachedBlockLogs { hash, logs });
        while self.blocks.len() > self.capacity {
            self.blocks.pop_front();
            if let Some(first_block) = &mut self.first_block {
                *first_block += 1;
            }
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
    provider: DynProvider,
    block_updates: watch::Receiver<BlockUpdates>,
    recent: Arc<RwLock<RecentLogs>>,
}

impl LogsCache {
    pub fn new(
        provider: DynProvider,
        block_updates: watch::Receiver<BlockUpdates>,
        capacity: usize,
    ) -> Self {
        Self {
            provider,
            block_updates,
            recent: Arc::new(RwLock::new(RecentLogs::new(capacity))),
        }
    }

    /// Identical to alloy's get_logs but with caching optimizations.
    pub async fn get_logs(&self, filter: &Filter) -> TransportResult<Vec<Log>> {
        let latest_snapshot = *self.block_updates.borrow();
        self.synchronize_if_needed(latest_snapshot).await?;

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
            METRICS.logs_cache_hits.inc();
            Ok(cached_logs)
        } else {
            METRICS.logs_cache_fallbacks.inc();
            self.provider.get_logs(filter).await
        }
    }

    /// If the chain head has changed, check for reorgs & add new blocks.
    ///
    /// We check for reverts if either latest or latest finalized has changed.
    /// This is not exact but it keeps the behavior consistent with how this worked previously.
    async fn synchronize_if_needed(&self, latest_snapshot: BlockUpdates) -> TransportResult<()> {
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

            let block = self
                .provider
                .get_block_by_number(BlockNumberOrTag::Number(block_number))
                .await?
                .ok_or_else(|| TransportErrorKind::custom_str("block not found"))?;
            if let Some(previous_number) = block_number.checked_sub(1).filter(|&n| n >= floor)
                && recent.cached_hash(previous_number) != Some(block.header.parent_hash)
            {
                tracing::warn!(
                    block_number,
                    previous_number,
                    "recent logs cache detected reorg"
                );
                self.update_block(recent, previous_number, floor).await?;
                // Instead of adding the block check for reorgs at `block_number` once again.
                self.update_block(recent, block_number, floor).await?;
                return Ok(());
            }

            let logs = self
                .provider
                .get_logs(&Filter::new().at_block_hash(block.header.hash))
                .await?;
            recent.push_head(block_number, block.header.hash, logs)?;
            METRICS.logs_cache_blocks_loaded.inc();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::types::{Block, Header};
    use alloy::transports::mock::Asserter;
    use alloy::{network::EthereumWallet, primitives::Address};

    fn mocked_provider() -> (DynProvider, Asserter) {
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .wallet(EthereumWallet::default())
            .connect_mocked_client(asserter.clone())
            .erased();
        (provider, asserter)
    }

    fn block_updates(latest_block: u64, finalized_block: u64) -> watch::Receiver<BlockUpdates> {
        let (_tx, rx) = watch::channel(BlockUpdates {
            latest_block,
            finalized_block,
        });
        rx
    }

    #[test]
    fn cached_logs_in_range_rejects_reversed_ranges() {
        let recent = RecentLogs {
            capacity: 128,
            synced_with: BlockUpdates {
                latest_block: 12,
                finalized_block: 0,
            },
            first_block: Some(10),
            blocks: VecDeque::from([
                CachedBlockLogs {
                    hash: B256::repeat_byte(10),
                    logs: vec![],
                },
                CachedBlockLogs {
                    hash: B256::repeat_byte(11),
                    logs: vec![],
                },
                CachedBlockLogs {
                    hash: B256::repeat_byte(12),
                    logs: vec![],
                },
            ]),
        };

        assert!(recent.cached_logs_in_range(12, 10).is_none());
    }

    #[tokio::test]
    async fn reversed_numeric_ranges_fall_back_to_provider() {
        let (provider, asserter) = mocked_provider();
        let expected_log = rpc_log(
            10,
            B256::repeat_byte(10),
            Address::repeat_byte(1),
            [B256::repeat_byte(2), B256::repeat_byte(3)],
        );
        asserter.push_success(&vec![expected_log.clone()]);
        let cache = LogsCache {
            provider,
            block_updates: block_updates(12, 0),
            recent: Arc::new(RwLock::new(RecentLogs {
                capacity: 128,
                synced_with: BlockUpdates {
                    latest_block: 12,
                    finalized_block: 0,
                },
                first_block: Some(10),
                blocks: VecDeque::from([CachedBlockLogs {
                    hash: B256::repeat_byte(10),
                    logs: vec![expected_log.clone()],
                }]),
            })),
        };

        let filter = Filter::new()
            .from_block(12u64)
            .to_block(10u64)
            .address(Address::repeat_byte(1))
            .event_signature(B256::repeat_byte(2))
            .topic1(B256::repeat_byte(3));

        let logs = cache
            .get_logs(&filter)
            .await
            .expect("reversed numeric ranges should fall back to the provider");

        assert_eq!(logs, vec![expected_log]);
        assert!(
            asserter.read_q().is_empty(),
            "provider fallback response should be consumed",
        );
    }

    #[test]
    fn exact_numeric_ranges_are_cache_hit_candidates() {
        let filter = Filter::new()
            .from_block(10u64)
            .to_block(12u64)
            .address(Address::repeat_byte(1))
            .event_signature(B256::repeat_byte(2))
            .topic1(B256::repeat_byte(3));
        assert_eq!(filter.extract_block_range(), (Some(10), Some(12)));
    }

    #[test]
    fn block_hash_queries_do_not_have_exact_numeric_ranges() {
        let filter = Filter::new()
            .at_block_hash(B256::repeat_byte(9))
            .address(Address::repeat_byte(1));
        assert_eq!(filter.extract_block_range(), (None, None));
    }

    #[tokio::test]
    async fn cached_range_hit_filters_logs_in_memory() {
        let (provider, _asserter) = mocked_provider();
        let cache = LogsCache {
            provider,
            block_updates: block_updates(12, 0),
            recent: Arc::new(RwLock::new(RecentLogs {
                capacity: 128,
                synced_with: BlockUpdates {
                    latest_block: 12,
                    finalized_block: 0,
                },
                first_block: Some(10),
                blocks: VecDeque::from([
                    CachedBlockLogs {
                        hash: B256::repeat_byte(10),
                        logs: vec![rpc_log(
                            10,
                            B256::repeat_byte(10),
                            Address::repeat_byte(1),
                            [B256::repeat_byte(2), B256::repeat_byte(3)],
                        )],
                    },
                    CachedBlockLogs {
                        hash: B256::repeat_byte(11),
                        logs: vec![rpc_log(
                            11,
                            B256::repeat_byte(11),
                            Address::repeat_byte(1),
                            [B256::repeat_byte(2), B256::repeat_byte(4)],
                        )],
                    },
                    CachedBlockLogs {
                        hash: B256::repeat_byte(12),
                        logs: vec![rpc_log(
                            12,
                            B256::repeat_byte(12),
                            Address::repeat_byte(2),
                            [B256::repeat_byte(2), B256::repeat_byte(3)],
                        )],
                    },
                ]),
            })),
        };

        let filter = Filter::new()
            .from_block(10u64)
            .to_block(12u64)
            .address(Address::repeat_byte(1))
            .event_signature(B256::repeat_byte(2))
            .topic1(B256::repeat_byte(3));

        let logs = cache
            .get_logs(&filter)
            .await
            .expect("cached query should succeed");

        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].block_number, Some(10));
    }

    #[tokio::test]
    async fn block_hash_queries_fall_back_to_provider() {
        let (provider, asserter) = mocked_provider();
        let expected_log = rpc_log(
            12,
            B256::repeat_byte(12),
            Address::repeat_byte(1),
            [B256::repeat_byte(2), B256::repeat_byte(3)],
        );
        asserter.push_success(&vec![expected_log.clone()]);
        let cache = LogsCache {
            provider,
            block_updates: block_updates(12, 0),
            recent: Arc::new(RwLock::new(RecentLogs {
                capacity: 128,
                synced_with: BlockUpdates {
                    latest_block: 12,
                    finalized_block: 0,
                },
                first_block: Some(10),
                blocks: VecDeque::from([CachedBlockLogs {
                    hash: B256::repeat_byte(12),
                    logs: vec![expected_log.clone()],
                }]),
            })),
        };

        let filter = Filter::new()
            .at_block_hash(B256::repeat_byte(12))
            .address(Address::repeat_byte(1));

        let logs = cache
            .get_logs(&filter)
            .await
            .expect("block hash queries should fall back to the provider");

        assert_eq!(logs, vec![expected_log]);
        assert!(
            asserter.read_q().is_empty(),
            "provider fallback response should be consumed",
        );
    }

    #[tokio::test]
    async fn partial_range_cache_miss_falls_back_to_provider() {
        let (provider, asserter) = mocked_provider();
        let expected_log = rpc_log(
            9,
            B256::repeat_byte(9),
            Address::repeat_byte(1),
            [B256::repeat_byte(2), B256::repeat_byte(3)],
        );
        asserter.push_success(&vec![expected_log.clone()]);
        let cache = LogsCache {
            provider,
            block_updates: block_updates(12, 0),
            recent: Arc::new(RwLock::new(RecentLogs {
                capacity: 128,
                synced_with: BlockUpdates {
                    latest_block: 12,
                    finalized_block: 0,
                },
                first_block: Some(10),
                blocks: VecDeque::from([CachedBlockLogs {
                    hash: B256::repeat_byte(10),
                    logs: vec![rpc_log(
                        10,
                        B256::repeat_byte(10),
                        Address::repeat_byte(1),
                        [B256::repeat_byte(2), B256::repeat_byte(3)],
                    )],
                }]),
            })),
        };

        let filter = Filter::new()
            .from_block(9u64)
            .to_block(10u64)
            .address(Address::repeat_byte(1))
            .event_signature(B256::repeat_byte(2))
            .topic1(B256::repeat_byte(3));

        let logs = cache
            .get_logs(&filter)
            .await
            .expect("partial cache misses should fall back to the provider");

        assert_eq!(logs, vec![expected_log]);
        assert!(
            asserter.read_q().is_empty(),
            "provider fallback response should be consumed",
        );
    }

    #[tokio::test]
    async fn reorg_sync_repairs_parent_before_refetching_current_block() {
        let (provider, asserter) = mocked_provider();
        let cache = LogsCache {
            provider,
            block_updates: block_updates(12, 0),
            recent: Arc::new(RwLock::new(RecentLogs {
                capacity: 128,
                synced_with: BlockUpdates {
                    latest_block: 11,
                    finalized_block: 0,
                },
                first_block: Some(10),
                blocks: VecDeque::from([
                    CachedBlockLogs {
                        hash: B256::repeat_byte(10),
                        logs: vec![rpc_log(
                            10,
                            B256::repeat_byte(10),
                            Address::repeat_byte(1),
                            [B256::repeat_byte(2), B256::repeat_byte(3)],
                        )],
                    },
                    CachedBlockLogs {
                        hash: B256::repeat_byte(0x11),
                        logs: vec![rpc_log(
                            11,
                            B256::repeat_byte(0x11),
                            Address::repeat_byte(1),
                            [B256::repeat_byte(2), B256::repeat_byte(3)],
                        )],
                    },
                    CachedBlockLogs {
                        hash: B256::repeat_byte(0x12),
                        logs: vec![rpc_log(
                            12,
                            B256::repeat_byte(0x12),
                            Address::repeat_byte(1),
                            [B256::repeat_byte(2), B256::repeat_byte(3)],
                        )],
                    },
                ]),
            })),
        };

        asserter.push_success(&Some(rpc_block(
            12,
            B256::repeat_byte(0x22),
            B256::repeat_byte(0x21),
        )));
        asserter.push_success(&Some(rpc_block(
            11,
            B256::repeat_byte(0x21),
            B256::repeat_byte(10),
        )));
        asserter.push_success(&vec![rpc_log(
            11,
            B256::repeat_byte(0x21),
            Address::repeat_byte(1),
            [B256::repeat_byte(2), B256::repeat_byte(3)],
        )]);
        asserter.push_success(&Some(rpc_block(
            12,
            B256::repeat_byte(0x22),
            B256::repeat_byte(0x21),
        )));
        asserter.push_success(&vec![rpc_log(
            12,
            B256::repeat_byte(0x22),
            Address::repeat_byte(1),
            [B256::repeat_byte(2), B256::repeat_byte(3)],
        )]);

        let filter = Filter::new()
            .from_block(10u64)
            .to_block(12u64)
            .address(Address::repeat_byte(1))
            .event_signature(B256::repeat_byte(2))
            .topic1(B256::repeat_byte(3));

        let logs = cache
            .get_logs(&filter)
            .await
            .expect("reorg repair should succeed");

        assert_eq!(logs.len(), 3);
        assert_eq!(logs[1].block_hash, Some(B256::repeat_byte(0x21)));
        assert_eq!(logs[2].block_hash, Some(B256::repeat_byte(0x22)));
        assert!(
            asserter.read_q().is_empty(),
            "the recursive reorg repair should consume exactly the prepared responses",
        );
    }

    #[tokio::test]
    async fn syncs_genesis_block_without_underflow() {
        let (provider, asserter) = mocked_provider();
        let cache = LogsCache {
            provider,
            block_updates: block_updates(0, 0),
            recent: Arc::new(RwLock::new(RecentLogs::new(128))),
        };

        asserter.push_success(&Some(rpc_block(0, B256::repeat_byte(0x10), B256::ZERO)));
        asserter.push_success(&vec![rpc_log(
            0,
            B256::repeat_byte(0x10),
            Address::repeat_byte(1),
            [B256::repeat_byte(2), B256::repeat_byte(3)],
        )]);

        let filter = Filter::new()
            .from_block(0u64)
            .to_block(0u64)
            .address(Address::repeat_byte(1))
            .event_signature(B256::repeat_byte(2))
            .topic1(B256::repeat_byte(3));

        let logs = cache
            .get_logs(&filter)
            .await
            .expect("genesis block sync should succeed");

        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].block_number, Some(0));
        assert!(
            asserter.read_q().is_empty(),
            "genesis sync should consume exactly one header and one logs response",
        );
    }

    fn rpc_log(block_number: u64, block_hash: B256, address: Address, topics: [B256; 2]) -> Log {
        let mut log: Log<alloy::primitives::LogData> = Log::default();
        log.inner =
            alloy::primitives::Log::new_unchecked(address, topics.into(), Default::default());
        log.block_number = Some(block_number);
        log.block_hash = Some(block_hash);
        log
    }

    #[allow(dead_code)]
    fn rpc_block(number: u64, hash: B256, parent_hash: B256) -> Block {
        let mut block = Block::default();
        block.header = Header {
            hash,
            inner: alloy::consensus::Header {
                number,
                parent_hash,
                ..Default::default()
            },
            total_difficulty: None,
            size: None,
        };
        block
    }
}
