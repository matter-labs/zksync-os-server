use crate::util;
use alloy::primitives::BlockNumber;
use alloy::providers::DynProvider;
use anyhow::Context;
use futures::stream::{self, StreamExt};
use rangemap::RangeInclusiveMap;
use reth_tasks::Runtime;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::sleep;
use zksync_os_batch_types::DiscoveredCommittedBatch;
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_contract_interface::settlement_layer_intervals::SettlementLayerIntervals;

const INIT_MAX_PARALLEL_BATCH_FETCHES: usize = 10;
const WAIT_FOR_BATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// In-memory store of committed batches discovered on startup and by the live commit watcher.
///
/// This component provides a single lookup / wait API for committed batch metadata regardless of
/// whether the batch came from startup catch-up or from the live `L1CommitWatcher`.
///
/// Depended on by:
/// - `L1ExecuteWatcher`, which waits for a committed batch before marking it executed;
/// - `Batcher`, which replays existing L1 batches before creating new ones;
/// - `PriorityTreeManager`, which reconstructs / advances the priority tree using committed batch
///   boundaries.
///
/// Construct it with [`Self::new`], which eagerly loads the startup frontier batches needed by
/// startup bookkeeping. Then run [`Self::init`] in a background task to populate the remaining
/// historical committed range while consumers use [`Self::wait_for_batch`] to block until a
/// specific batch becomes available.
#[derive(Debug, Clone)]
pub struct CommittedBatchProvider {
    inner: Arc<RwLock<Inner>>,
    /// Intervals used to route batch lookups to the diamond proxy of the SL the batch was
    /// committed to.
    intervals: SettlementLayerIntervals,
}

#[derive(Debug, Default)]
struct Inner {
    batches: HashMap<u64, DiscoveredCommittedBatch>,
    block_range_index: RangeInclusiveMap<BlockNumber, u64>,
}

impl CommittedBatchProvider {
    /// Creates a provider, inserts the genesis batch if needed, and eagerly loads the startup
    /// frontier batches used by startup bookkeeping.
    pub async fn new(
        runtime: &Runtime,
        l1_state: &L1State,
        max_l1_blocks_to_scan: u64,
        load_genesis_batch_info: impl AsyncFnOnce() -> StoredBatchInfo,
    ) -> anyhow::Result<Self> {
        let provider = Self {
            inner: Arc::new(RwLock::new(Inner::default())),
            intervals: l1_state.settlement_layer_intervals.clone(),
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

        let (prioritized_batch_numbers, _) = startup_batch_numbers(
            l1_state.last_committed_batch,
            l1_state.last_proved_batch,
            l1_state.last_executed_batch,
        );
        provider
            .load_batch_numbers(max_l1_blocks_to_scan, prioritized_batch_numbers)
            .await?;

        let provider_for_init = provider.clone();
        let last_committed = l1_state.last_committed_batch;
        let last_proved = l1_state.last_proved_batch;
        let last_executed = l1_state.last_executed_batch;
        runtime.spawn_critical_task("committed batch provider init", async move {
            provider_for_init
                .init(
                    last_committed,
                    last_proved,
                    last_executed,
                    max_l1_blocks_to_scan,
                )
                .await
                .expect("failed to initialize CommittedBatchProvider");
        });

        Ok(provider)
    }

    /// Loads the remaining historical committed batches discovered on startup.
    async fn init(
        &self,
        last_committed_batch: u64,
        last_proved_batch: u64,
        last_executed_batch: u64,
        max_l1_blocks_to_scan: u64,
    ) -> anyhow::Result<()> {
        let (_, remaining_batch_numbers) =
            startup_batch_numbers(last_committed_batch, last_proved_batch, last_executed_batch);
        self.load_batch_numbers(max_l1_blocks_to_scan, remaining_batch_numbers)
            .await?;
        Ok(())
    }

    pub(crate) fn insert(&self, batch: DiscoveredCommittedBatch) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.insert(batch);
    }

    /// Waits until the requested batch is available in memory.
    ///
    /// Startup initialization and live L1 watchers both populate this provider, so callers can use
    /// a single API regardless of whether the batch is historical or just arrived from L1.
    pub async fn wait_for_batch(&self, batch_number: u64) -> DiscoveredCommittedBatch {
        let mut logged_wait = false;
        loop {
            let batch = {
                let inner = self.inner.read().expect("lock poisoned");
                inner.batches.get(&batch_number).cloned()
            };
            if let Some(batch) = batch {
                tracing::info!("returning batch {batch_number} from CommittedBatchProvider");
                return batch;
            }
            if !logged_wait {
                tracing::info!("waiting for committed batch {batch_number} to load");
                logged_wait = true;
            }
            sleep(WAIT_FOR_BATCH_POLL_INTERVAL).await;
        }
    }

    /// Returns `DiscoveredCommittedBatch` from in-memory map if available.
    pub fn get(&self, batch_number: u64) -> Option<DiscoveredCommittedBatch> {
        let inner = self.inner.read().expect("lock poisoned");
        inner.batches.get(&batch_number).cloned()
    }

    /// Fetches a batch set with bounded concurrency to reduce startup latency without issuing an
    /// unbounded number of L1 requests.
    async fn load_batch_numbers(
        &self,
        max_l1_blocks_to_scan: u64,
        batch_numbers: Vec<u64>,
    ) -> anyhow::Result<()> {
        stream::iter(batch_numbers)
            .map(|batch_number| async move {
                let proxy = self.intervals.resolve_proxy(batch_number)?;
                let discovered_batch =
                    fetch_batch(proxy, batch_number, max_l1_blocks_to_scan).await?;
                tracing::info!(
                    batch_number = discovered_batch.number(),
                    "discovered committed batch {} on startup",
                    discovered_batch.number()
                );
                self.insert(discovered_batch);
                anyhow::Ok(())
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

/// Returns startup frontier batches first, then the remaining committed startup range.
///
/// The prioritized vector preserves the bookkeeping order most likely to unblock startup:
/// committed, proved, then executed.
fn startup_batch_numbers(
    last_committed_batch: u64,
    last_proved_batch: u64,
    last_executed_batch: u64,
) -> (Vec<u64>, Vec<u64>) {
    let prioritized = [last_committed_batch, last_proved_batch, last_executed_batch];
    let (prioritized_in_range, remaining_batch_numbers): (Vec<_>, Vec<_>) =
        (last_executed_batch.max(1)..=last_committed_batch)
            .partition(|batch_number| prioritized.contains(batch_number));

    (prioritized_in_range, remaining_batch_numbers)
}

/// Resolves a committed batch from L1 by first finding the block that committed it and then
/// decoding the corresponding stored batch data.
async fn fetch_batch(
    diamond_proxy_sl: &ZkChain<DynProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<DiscoveredCommittedBatch> {
    let sl_block_with_commit = util::find_l1_commit_block_by_batch_number(
        diamond_proxy_sl.clone(),
        batch_number,
        max_l1_blocks_to_scan,
    )
    .await?;
    util::fetch_stored_batch_data(diamond_proxy_sl, sl_block_with_commit, batch_number)
        .await?
        .with_context(|| format!("failed to find committed batch {batch_number} on L1"))
}

#[cfg(test)]
mod tests {
    use super::startup_batch_numbers;

    #[test]
    fn prioritizes_frontier_batches_once() {
        assert_eq!(startup_batch_numbers(10, 8, 8), (vec![8, 10], vec![9]));
    }

    #[test]
    fn excludes_prioritized_batches_from_remaining_range() {
        assert_eq!(
            startup_batch_numbers(10, 8, 6),
            (vec![6, 8, 10], vec![7, 9])
        );
    }
}
