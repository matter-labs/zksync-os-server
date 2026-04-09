use crate::util;
use alloy::primitives::BlockNumber;
use alloy::providers::DynProvider;
use anyhow::Context;
use futures::stream::{self, StreamExt};
use rangemap::RangeInclusiveMap;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::sleep;
use zksync_os_batch_types::DiscoveredCommittedBatch;
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_contract_interface::models::StoredBatchInfo;

const INIT_MAX_PARALLEL_BATCH_FETCHES: usize = 10;
const WAIT_FOR_BATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct CommittedBatchProvider {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    batches: HashMap<u64, DiscoveredCommittedBatch>,
    block_range_index: RangeInclusiveMap<BlockNumber, u64>,
}

impl CommittedBatchProvider {
    pub async fn new(
        l1_state: &L1State,
        load_genesis_batch_info: impl AsyncFnOnce() -> StoredBatchInfo,
    ) -> anyhow::Result<Self> {
        let provider = Self {
            inner: Arc::new(RwLock::new(Inner::default())),
        };
        // Special case for genesis
        if l1_state.last_executed_batch == 0 {
            let batch_info = load_genesis_batch_info().await;
            let batch_hash_l1 = l1_state.diamond_proxy_l1.stored_batch_hash(0).await?;
            anyhow::ensure!(
                batch_hash_l1 == batch_info.hash(),
                "genesis batch hash mismatch: L1 {}, local {}",
                batch_hash_l1,
                batch_info.hash(),
            );
            provider.insert(DiscoveredCommittedBatch {
                batch_info,
                block_range: 0..=0,
            });
        }

        Ok(provider)
    }

    pub async fn init(&self, l1_state: &L1State, max_l1_blocks_to_scan: u64) -> anyhow::Result<()> {
        self.load_batches_in_background(
            l1_state.diamond_proxy_sl.clone(),
            max_l1_blocks_to_scan,
            startup_priority_batch_numbers(
                l1_state.last_committed_batch,
                l1_state.last_proved_batch,
                l1_state.last_executed_batch,
            ),
            startup_remaining_batch_numbers(
                l1_state.last_committed_batch,
                l1_state.last_proved_batch,
                l1_state.last_executed_batch,
            ),
        )
        .await
    }

    pub(crate) fn insert(&self, batch: DiscoveredCommittedBatch) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.insert(batch);
    }

    pub async fn wait_for_batch(&self, batch_number: u64) -> DiscoveredCommittedBatch {
        let mut logged_wait = false;
        loop {
            let batch = {
                let inner = self.inner.read().expect("lock poisoned");
                inner.batches.get(&batch_number).cloned()
            };
            if let Some(batch) = batch {
                return batch;
            }
            if !logged_wait {
                tracing::info!("waiting for committed batch {batch_number} to load");
                logged_wait = true;
            }
            sleep(WAIT_FOR_BATCH_POLL_INTERVAL).await;
        }
    }

    async fn load_batches_in_background(
        &self,
        diamond_proxy_sl: ZkChain<DynProvider>,
        max_l1_blocks_to_scan: u64,
        prioritized_batch_numbers: Vec<u64>,
        remaining_batch_numbers: Vec<u64>,
    ) -> anyhow::Result<()> {
        self.load_batch_numbers(
            diamond_proxy_sl.clone(),
            max_l1_blocks_to_scan,
            prioritized_batch_numbers,
        )
        .await?;
        self.load_batch_numbers(
            diamond_proxy_sl,
            max_l1_blocks_to_scan,
            remaining_batch_numbers,
        )
        .await?;
        Ok(())
    }

    async fn load_batch_numbers(
        &self,
        diamond_proxy_sl: ZkChain<DynProvider>,
        max_l1_blocks_to_scan: u64,
        batch_numbers: Vec<u64>,
    ) -> anyhow::Result<()> {
        stream::iter(batch_numbers)
            .map(|batch_number| {
                let provider = self.clone();
                let diamond_proxy_sl = diamond_proxy_sl.clone();
                async move {
                    let discovered_batch =
                        fetch_batch(diamond_proxy_sl, batch_number, max_l1_blocks_to_scan).await?;
                    tracing::info!(
                        "discovered committed batch {} on startup",
                        discovered_batch.number()
                    );
                    provider.insert(discovered_batch);
                    Ok::<_, anyhow::Error>(())
                }
            })
            .buffer_unordered(INIT_MAX_PARALLEL_BATCH_FETCHES)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(())
    }
}

impl Inner {
    fn insert(&mut self, batch: DiscoveredCommittedBatch) {
        self.block_range_index
            .insert(batch.block_range.clone(), batch.number());
        self.batches.insert(batch.number(), batch);
    }
}

fn startup_priority_batch_numbers(
    last_committed_batch: u64,
    last_proved_batch: u64,
    last_executed_batch: u64,
) -> Vec<u64> {
    let mut seen = HashSet::new();
    [last_committed_batch, last_proved_batch, last_executed_batch]
        .into_iter()
        .filter(|batch_number| *batch_number > 0)
        .filter(|batch_number| seen.insert(*batch_number))
        .collect()
}

fn startup_remaining_batch_numbers(
    last_committed_batch: u64,
    last_proved_batch: u64,
    last_executed_batch: u64,
) -> Vec<u64> {
    let prioritized: HashSet<_> = startup_priority_batch_numbers(
        last_committed_batch,
        last_proved_batch,
        last_executed_batch,
    )
    .into_iter()
    .collect();
    (last_executed_batch.max(1)..=last_committed_batch)
        .filter(|batch_number| !prioritized.contains(batch_number))
        .collect()
}

async fn fetch_batch(
    diamond_proxy_sl: ZkChain<DynProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<DiscoveredCommittedBatch> {
    let sl_block_with_commit = util::find_l1_commit_block_by_batch_number(
        diamond_proxy_sl.clone(),
        batch_number,
        max_l1_blocks_to_scan,
    )
    .await?;
    util::fetch_stored_batch_data(&diamond_proxy_sl, sl_block_with_commit, batch_number)
        .await?
        .with_context(|| format!("failed to find committed batch {batch_number} on L1"))
}

#[cfg(test)]
mod tests {
    use super::{startup_priority_batch_numbers, startup_remaining_batch_numbers};

    #[test]
    fn prioritizes_frontier_batches_once() {
        assert_eq!(startup_priority_batch_numbers(10, 8, 8), vec![10, 8]);
    }

    #[test]
    fn excludes_prioritized_batches_from_remaining_range() {
        assert_eq!(startup_remaining_batch_numbers(10, 8, 6), vec![7, 9]);
    }
}
