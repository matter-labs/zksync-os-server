use crate::util;
use alloy::primitives::BlockNumber;
use alloy::providers::DynProvider;
use anyhow::Context;
use futures::stream::{self, StreamExt};
use rangemap::RangeInclusiveMap;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::sleep;
use zksync_os_batch_types::DiscoveredCommittedBatch;
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_storage_api::ReadBatch;

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
#[derive(Clone)]
pub struct CommittedBatchProvider {
    inner: Arc<RwLock<Inner>>,
    /// Final fallback for resolving commit events that live on a settlement layer this node can
    /// no longer query (e.g. after migrating a chain back from a gateway — commits for
    /// pre-migration batches are on the gateway L2 diamond). Written by the previous run's
    /// `L1PersistBatchWatcher`.
    local_batch_storage: Arc<dyn ReadBatch>,
}

// Manual Debug impl because `dyn ReadBatch` isn't Debug.
impl std::fmt::Debug for CommittedBatchProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommittedBatchProvider")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct Inner {
    batches: HashMap<u64, DiscoveredCommittedBatch>,
    block_range_index: RangeInclusiveMap<BlockNumber, u64>,
}

impl CommittedBatchProvider {
    /// Creates a provider, inserts the genesis batch if needed, and eagerly loads the startup
    /// frontier batches used by startup bookkeeping.
    ///
    /// `local_batch_storage` is consulted as a final fallback when a batch's commit event can't
    /// be found on either the current SL or L1 diamond. This is essential for recovery after a
    /// live settlement-layer migration:
    ///   - to-gateway: commits for pre-migration batches live on the chain's L1 diamond. The
    ///     existing `diamond_proxy_l1` fallback resolves them.
    ///   - from-gateway: commits for pre-migration batches live on the chain's diamond on the
    ///     former-gateway L2, which the server can't (and shouldn't have to) query on restart.
    ///     The `L1PersistBatchWatcher` on the previous run wrote those batches to local RocksDB,
    ///     so we read them back from there.
    ///
    /// For a chain that never migrated, the fallback is never hit: the SL lookup succeeds on the
    /// first try.
    pub async fn new(
        l1_state: &L1State,
        max_l1_blocks_to_scan: u64,
        load_genesis_batch_info: impl AsyncFnOnce() -> StoredBatchInfo,
        local_batch_storage: Arc<dyn ReadBatch>,
    ) -> anyhow::Result<Self> {
        let provider = Self {
            inner: Arc::new(RwLock::new(Inner::default())),
            local_batch_storage,
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
            .load_batch_numbers(
                l1_state.diamond_proxy_sl.clone(),
                l1_state.diamond_proxy_l1.clone(),
                max_l1_blocks_to_scan,
                prioritized_batch_numbers,
            )
            .await?;

        Ok(provider)
    }

    /// Loads the remaining historical committed batches discovered on startup.
    pub async fn init(&self, l1_state: &L1State, max_l1_blocks_to_scan: u64) -> anyhow::Result<()> {
        let (_, remaining_batch_numbers) = startup_batch_numbers(
            l1_state.last_committed_batch,
            l1_state.last_proved_batch,
            l1_state.last_executed_batch,
        );
        self.load_batch_numbers(
            l1_state.diamond_proxy_sl.clone(),
            l1_state.diamond_proxy_l1.clone(),
            max_l1_blocks_to_scan,
            remaining_batch_numbers,
        )
        .await?;
        Ok(())
    }


    pub fn insert(&self, batch: DiscoveredCommittedBatch) {
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
        diamond_proxy_sl: ZkChain<DynProvider>,
        diamond_proxy_l1: ZkChain<DynProvider>,
        max_l1_blocks_to_scan: u64,
        batch_numbers: Vec<u64>,
    ) -> anyhow::Result<()> {
        stream::iter(batch_numbers)
            .map(|batch_number| {
                let provider = self.clone();
                let diamond_proxy_sl = diamond_proxy_sl.clone();
                let diamond_proxy_l1 = diamond_proxy_l1.clone();
                async move {
                    let discovered_batch = fetch_batch(
                        diamond_proxy_sl,
                        diamond_proxy_l1,
                        batch_number,
                        max_l1_blocks_to_scan,
                        provider.local_batch_storage.as_ref(),
                    )
                    .await?;
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

/// Resolves a committed batch by cascading through three sources in order of freshness:
///   1. `diamond_proxy_sl` — the current settlement layer diamond, canonical for new commits.
///   2. `diamond_proxy_l1` — the chain's L1 diamond. Distinct from `diamond_proxy_sl` only for
///      a gateway-settling chain; holds commits that pre-date a *to-gateway* migration.
///   3. `local_batch_storage` — local RocksDB populated by the previous run's
///      `L1PersistBatchWatcher`. The only way to recover commit metadata after a
///      *from-gateway* migration, because those commits live on the former-gateway L2
///      diamond which this node can no longer (and shouldn't need to) query.
///
/// For a chain that never migrated, sources 2 and 3 are never hit: source 1 resolves.
async fn fetch_batch(
    diamond_proxy_sl: ZkChain<DynProvider>,
    diamond_proxy_l1: ZkChain<DynProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
    local_batch_storage: &dyn ReadBatch,
) -> anyhow::Result<DiscoveredCommittedBatch> {
    if let Some(batch) =
        try_fetch_batch_on_proxy(&diamond_proxy_sl, batch_number, max_l1_blocks_to_scan).await?
    {
        return Ok(batch);
    }
    if let Some(batch) =
        try_fetch_batch_on_proxy(&diamond_proxy_l1, batch_number, max_l1_blocks_to_scan).await?
    {
        return Ok(batch);
    }
    match local_batch_storage.get_batch_by_number(batch_number) {
        Ok(Some(persisted)) => {
            tracing::info!(
                batch_number,
                "resolved committed batch from local storage after SL + L1 diamond missed \
                 (post-migration recovery path)",
            );
            Ok(persisted.committed_batch)
        }
        Ok(None) => anyhow::bail!(
            "failed to find committed batch {batch_number} on current SL, L1 diamond, \
             or local storage",
        ),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to find committed batch {batch_number} on SL or L1 diamond, and \
                 local storage lookup errored",
            )
        }),
    }
}

async fn try_fetch_batch_on_proxy(
    zk_chain: &ZkChain<DynProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<Option<DiscoveredCommittedBatch>> {
    let Some(block_with_commit) = util::find_l1_commit_block_by_batch_number(
        zk_chain.clone(),
        batch_number,
        max_l1_blocks_to_scan,
    )
    .await?
    else {
        return Ok(None);
    };
    util::fetch_stored_batch_data(zk_chain, block_with_commit, batch_number).await
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
