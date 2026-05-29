use crate::{BlockUpdates, metrics::METRICS};
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{B256, BlockNumber};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Block, Filter, Log};
use alloy::transports::{TransportErrorKind, TransportResult};
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

#[derive(Debug)]
struct RecentBlocks {
    synced_with: BlockUpdates,
    first_block: Option<u64>,
    blocks: VecDeque<CachedBlockLogs>,
}

impl Default for RecentBlocks {
    fn default() -> Self {
        Self {
            synced_with: UNSYNCED_BLOCK_UPDATES,
            first_block: None,
            blocks: VecDeque::new(),
        }
    }
}

impl RecentBlocks {
    fn latest_block(&self) -> Option<u64> {
        self.first_block
            .zip(self.blocks.len().checked_sub(1))
            .map(|(first_block, last_offset)| first_block + last_offset as u64)
    }

    fn contains_range(&self, from_block: u64, to_block: u64) -> bool {
        let Some(first_block) = self.first_block else {
            return false;
        };
        let Some(last_block) = self.latest_block() else {
            return false;
        };
        from_block >= first_block && to_block <= last_block
    }

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

    fn push_block(&mut self, number: u64, hash: B256, logs: Vec<Log>, capacity: usize) {
        if self.blocks.is_empty() {
            self.first_block = Some(number);
        }
        self.blocks.push_back(CachedBlockLogs { hash, logs });

        while self.blocks.len() > capacity {
            self.blocks.pop_front();
            if let Some(first_block) = &mut self.first_block {
                *first_block += 1;
            }
        }
        if self.blocks.is_empty() {
            self.first_block = None;
        }
    }

    fn pop_back(&mut self) -> Option<CachedBlockLogs> {
        let result = self.blocks.pop_back();
        if self.blocks.is_empty() {
            self.first_block = None;
        }
        result
    }

    fn truncate_latest_to(&mut self, to_block: u64) {
        while self
            .latest_block()
            .is_some_and(|latest_block| latest_block > to_block)
        {
            self.pop_back();
        }
    }
}

#[derive(Debug)]
struct CacheEligibility {
    is_cache_hit_candidate: bool,
    from_block: u64,
    to_block: u64,
}

impl CacheEligibility {
    fn for_filter(filter: &Filter, first_cached_block: u64, last_cached_block: u64) -> Self {
        let supported_topics = filter.topics[2].is_empty() && filter.topics[3].is_empty();
        let block_hash = filter.get_block_hash().is_none();
        let (from_block, to_block) = filter.extract_block_range();

        let is_cache_hit_candidate = supported_topics
            && block_hash
            && filter.address.to_value_or_array().is_some()
            && from_block
                .zip(to_block)
                .is_some_and(|(from_block, to_block)| {
                    from_block >= first_cached_block && to_block <= last_cached_block
                });

        Self {
            is_cache_hit_candidate,
            from_block: from_block.unwrap_or_default(),
            to_block: to_block.unwrap_or_default(),
        }
    }
}

#[derive(Debug)]
pub struct LogsCache {
    provider: DynProvider,
    block_updates: watch::Receiver<BlockUpdates>,
    recent: RwLock<RecentBlocks>,
    capacity: usize,
}

impl LogsCache {
    pub fn new(
        provider: DynProvider,
        block_updates: watch::Receiver<BlockUpdates>,
        capacity: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            provider,
            block_updates,
            recent: RwLock::new(RecentBlocks::default()),
            capacity,
        })
    }

    pub async fn get_logs(&self, filter: &Filter) -> TransportResult<Vec<Log>> {
        let latest_snapshot = *self.block_updates.borrow();
        self.synchronize_if_needed(latest_snapshot).await?;

        let cached_logs = {
            let recent = self.recent.read().await;
            match recent.first_block.zip(recent.latest_block()) {
                Some((first_cached_block, last_cached_block)) => {
                    let eligibility =
                        CacheEligibility::for_filter(filter, first_cached_block, last_cached_block);
                    if !eligibility.is_cache_hit_candidate {
                        None
                    } else {
                        Some(
                            recent
                                .cached_logs_in_range(eligibility.from_block, eligibility.to_block)
                                .expect("eligibility guarantees full cached range coverage")
                                .filter(|log| filter.rpc_matches(log))
                                .cloned()
                                .collect(),
                        )
                    }
                }
                None => None,
            }
        };

        if let Some(cached_logs) = cached_logs {
            METRICS.logs_cache_hits.inc();
            Ok(cached_logs)
        } else {
            METRICS.logs_cache_fallbacks.inc();
            self.provider.get_logs(filter).await
        }
    }

    async fn synchronize_if_needed(&self, latest_snapshot: BlockUpdates) -> TransportResult<()> {
        {
            let recent = self.recent.read().await;
            if recent.synced_with == latest_snapshot {
                return Ok(());
            }
        }

        let mut recent = self.recent.write().await;
        if recent.synced_with == latest_snapshot {
            return Ok(());
        }

        self.synchronize_locked(&mut recent, latest_snapshot)
            .await?;
        recent.synced_with = latest_snapshot;
        Ok(())
    }

    async fn synchronize_locked(
        &self,
        recent: &mut RecentBlocks,
        latest_snapshot: BlockUpdates,
    ) -> TransportResult<()> {
        let target_head = latest_snapshot.latest_block;
        if self.capacity == 0 {
            recent.blocks.clear();
            recent.first_block = None;
            return Ok(());
        }

        if recent.blocks.is_empty() {
            let start_block = target_head.saturating_sub(self.capacity as u64 - 1);
            self.fill_blocks(recent, start_block, target_head).await?;
            return Ok(());
        }

        recent.truncate_latest_to(target_head);
        if recent.blocks.is_empty() {
            return Err(TransportErrorKind::custom_str(
                "recent logs cache could not find common ancestor within retained window",
            ));
        }

        let mut rewind_depth = 0;
        while let Some(cached_tip_block) = recent.latest_block() {
            let canonical_hash = self.fetch_block_hash(cached_tip_block).await?;
            let cached_hash = recent
                .blocks
                .back()
                .expect("latest_block implies cache is not empty")
                .hash;
            if canonical_hash == cached_hash {
                break;
            }
            recent.pop_back();
            rewind_depth += 1;
        }

        if rewind_depth > 0 {
            if recent.blocks.is_empty() {
                return Err(TransportErrorKind::custom_str(
                    "recent logs cache could not find common ancestor within retained window",
                ));
            }
            METRICS.logs_cache_reorg_rewinds.inc();
            METRICS.logs_cache_reorg_rewind_depth.inc_by(rewind_depth);
            tracing::warn!(
                rewind_depth,
                target_head,
                "recent logs cache detected reorg"
            );
        }

        let start_block = recent.latest_block().map_or(
            target_head.saturating_sub(self.capacity as u64 - 1),
            |last_cached| last_cached.saturating_add(1),
        );
        self.fill_blocks(recent, start_block, target_head).await?;
        Ok(())
    }

    async fn fill_blocks(
        &self,
        recent: &mut RecentBlocks,
        from_block: u64,
        to_block: u64,
    ) -> TransportResult<()> {
        if from_block > to_block {
            return Ok(());
        }

        for block_number in from_block..=to_block {
            let block = self.fetch_block(block_number).await?;
            let logs = self.fetch_block_logs(block.header.hash).await?;
            recent.push_block(block_number, block.header.hash, logs, self.capacity);
            METRICS.logs_cache_blocks_loaded.inc();
        }
        Ok(())
    }

    async fn fetch_block(&self, block_number: u64) -> TransportResult<Block> {
        self.provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .await?
            .ok_or_else(|| TransportErrorKind::custom_str("block not found"))
    }

    async fn fetch_block_hash(&self, block_number: u64) -> TransportResult<B256> {
        Ok(self.fetch_block(block_number).await?.header.hash)
    }

    async fn fetch_block_logs(&self, block_hash: B256) -> TransportResult<Vec<Log>> {
        self.provider
            .get_logs(&Filter::new().at_block_hash(block_hash))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::types::Header;
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
    fn cache_eligibility_accepts_bounded_watcher_style_filters() {
        let filter = Filter::new()
            .from_block(10u64)
            .to_block(12u64)
            .address(Address::repeat_byte(1))
            .event_signature(B256::repeat_byte(2))
            .topic1(B256::repeat_byte(3));

        let eligibility = CacheEligibility::for_filter(&filter, 10, 12);

        assert!(eligibility.is_cache_hit_candidate);
    }

    #[test]
    fn cache_eligibility_rejects_block_hash_queries() {
        let filter = Filter::new()
            .at_block_hash(B256::repeat_byte(9))
            .address(Address::repeat_byte(1));

        let eligibility = CacheEligibility::for_filter(&filter, 0, 0);

        assert!(!eligibility.is_cache_hit_candidate);
    }

    #[tokio::test]
    async fn cached_range_hit_filters_logs_in_memory() {
        let (provider, _asserter) = mocked_provider();
        let cache = LogsCache {
            provider,
            block_updates: block_updates(12, 0),
            recent: RwLock::new(RecentBlocks {
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
            }),
            capacity: 128,
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
            recent: RwLock::new(RecentBlocks {
                synced_with: BlockUpdates {
                    latest_block: 12,
                    finalized_block: 0,
                },
                first_block: Some(10),
                blocks: VecDeque::from([CachedBlockLogs {
                    hash: B256::repeat_byte(12),
                    logs: vec![expected_log.clone()],
                }]),
            }),
            capacity: 128,
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
            recent: RwLock::new(RecentBlocks {
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
            }),
            capacity: 128,
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

    fn rpc_log(block_number: u64, block_hash: B256, address: Address, topics: [B256; 2]) -> Log {
        let mut log: Log<alloy::primitives::LogData> = Log::default();
        log.inner =
            alloy::primitives::Log::new_unchecked(address, topics.into(), Default::default());
        log.block_number = Some(block_number);
        log.block_hash = Some(block_hash);
        log
    }

    #[allow(dead_code)]
    fn rpc_block(number: u64, hash: B256) -> Block {
        let mut block = Block::default();
        block.header = Header {
            hash,
            inner: alloy::consensus::Header {
                number,
                ..Default::default()
            },
            total_difficulty: None,
            size: None,
        };
        block
    }
}
