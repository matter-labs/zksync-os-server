use crate::util;
use alloy::eips::BlockId;
use alloy::primitives::{B256, BlockNumber, TxHash};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context;
use futures::stream::{self, StreamExt};
use rangemap::RangeInclusiveMap;
use reth_tasks::Runtime;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::sleep;
use zksync_os_batch_types::{CommittedBatchInfo, DiscoveredCommittedBatch};
use zksync_os_contract_interface::IExecutor;
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_contract_interface::models::StoredBatchInfo;
use zksync_os_provider::NodeProvider;

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
/// Construct it with [`Self::new`], which locates every startup batch's commit with a single
/// `eth_getLogs` sweep and eagerly loads the startup frontier batches needed by startup
/// bookkeeping. The remaining historical committed range is populated by a background task while
/// consumers use [`Self::wait_for_batch`] to block until a specific batch becomes available.
#[derive(Debug, Clone)]
pub struct CommittedBatchProvider {
    inner: Arc<RwLock<Inner>>,
    /// L1 diamond proxy used to look up committed batches.
    diamond_proxy_l1: ZkChain<NodeProvider>,
}

#[derive(Debug, Default)]
struct Inner {
    batches: HashMap<u64, DiscoveredCommittedBatch>,
    block_range_index: RangeInclusiveMap<BlockNumber, u64>,
    /// L1 block where each batch's commit was observed during the startup sweep. Not populated
    /// for batches inserted by live watchers — no consumer needs those.
    commit_locations: HashMap<u64, BlockNumber>,
}

impl CommittedBatchProvider {
    /// Creates a provider, inserts the genesis batch if needed, and eagerly loads the startup
    /// frontier batches used by startup bookkeeping.
    pub async fn new(
        runtime: &Runtime,
        l1_state: &L1State,
        max_l1_blocks_per_logs_query: u64,
        load_genesis_batch_info: impl AsyncFnOnce() -> StoredBatchInfo,
    ) -> anyhow::Result<Self> {
        let provider = Self {
            inner: Arc::new(RwLock::new(Inner::default())),
            diamond_proxy_l1: l1_state.diamond_proxy_l1.clone(),
        };
        // Special case for genesis
        if l1_state.last_executed_batch == 0 {
            let batch_info = load_genesis_batch_info().await;
            let batch_hash_l1 = l1_state
                .diamond_proxy_l1
                .stored_batch_hash(0, BlockId::latest())
                .await?;
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

        let (prioritized_batch_numbers, remaining_batch_numbers) = startup_batch_numbers(
            l1_state.last_committed_batch,
            l1_state.last_proved_batch,
            l1_state.last_executed_batch,
            l1_state.last_finalized_executed_batch,
        );
        // One sweep covers both load phases: the prioritized set contains the extremes of the
        // startup range (finalized executed = min, committed = max), so the swept window spans
        // every startup batch's live commit.
        let swept = Arc::new(
            sweep_startup_commits(
                &provider.diamond_proxy_l1,
                max_l1_blocks_per_logs_query,
                prioritized_batch_numbers
                    .iter()
                    .chain(&remaining_batch_numbers)
                    .copied(),
            )
            .await?,
        );
        provider
            .load_swept_batches(&swept, prioritized_batch_numbers)
            .await?;

        let provider_for_init = provider.clone();
        runtime.spawn_critical_task("committed batch provider init", async move {
            provider_for_init
                .load_swept_batches(&swept, remaining_batch_numbers)
                .await
                .expect("failed to initialize CommittedBatchProvider");
        });

        Ok(provider)
    }

    pub(crate) fn insert(&self, batch: DiscoveredCommittedBatch) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.insert(batch);
    }

    fn insert_with_commit_location(
        &self,
        batch: DiscoveredCommittedBatch,
        commit_l1_block: BlockNumber,
    ) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner
            .commit_locations
            .insert(batch.number(), commit_l1_block);
        inner.insert(batch);
    }

    /// L1 block in which `batch_number`'s commit was observed during the startup sweep.
    pub fn commit_l1_block(&self, batch_number: u64) -> Option<BlockNumber> {
        let inner = self.inner.read().expect("lock poisoned");
        inner.commit_locations.get(&batch_number).copied()
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

    /// Assembles and stores a batch set from swept commit events with bounded concurrency: per
    /// batch only the commit transaction (for its calldata) and a `storedBatchHash` liveness
    /// check remain to be fetched.
    async fn load_swept_batches(
        &self,
        swept: &HashMap<u64, SweptCommit>,
        batch_numbers: Vec<u64>,
    ) -> anyhow::Result<()> {
        stream::iter(batch_numbers)
            .map(|batch_number| async move {
                let swept_commit = swept.get(&batch_number).with_context(|| {
                    format!(
                        "batch {batch_number} not found in the L1 commit sweep \
                         (reverted after startup state was read?)"
                    )
                })?;
                let commit_info = util::fetch_commit_batch_info(
                    &self.diamond_proxy_l1,
                    swept_commit.tx_hash,
                    batch_number,
                )
                .await?;
                let batch_info = CommittedBatchInfo {
                    commit_info,
                    commitment: swept_commit.commitment,
                }
                .into_stored();
                // The sweep cannot tell a live commit from one that was reverted and never
                // re-committed; the live `storedBatchHash` can. Also validates the calldata
                // decoding end to end.
                let live_hash = self
                    .diamond_proxy_l1
                    .stored_batch_hash(batch_number, BlockId::latest())
                    .await?;
                anyhow::ensure!(
                    batch_info.hash() == live_hash,
                    "batch {batch_number} reconstructed from the L1 commit sweep does not hash \
                     to the live `storedBatchHash` value {live_hash}",
                );
                tracing::info!(
                    batch_number,
                    "discovered committed batch {batch_number} on startup"
                );
                self.insert_with_commit_location(
                    DiscoveredCommittedBatch {
                        batch_info,
                        block_range: swept_commit.block_range.clone(),
                    },
                    swept_commit.l1_block_number,
                );
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

/// Locates the live commits of all `batch_numbers` with one predicate search and one chunked
/// `eth_getLogs` sweep.
///
/// The swept window starts at the *oldest* requested batch's commit block — for startup, the
/// last finalized executed batch — so a long execution/finalization stall blocks node startup
/// proportionally (see [`sweep_committed_batch_events`] for the cost model).
async fn sweep_startup_commits(
    diamond_proxy_l1: &ZkChain<NodeProvider>,
    max_blocks_per_query: u64,
    batch_numbers: impl Iterator<Item = u64>,
) -> anyhow::Result<HashMap<u64, SweptCommit>> {
    let (Some(min_batch), Some(max_batch)) = batch_numbers.fold((None, None), |(min, max), n| {
        (Some(n.min(min.unwrap_or(n))), Some(n.max(max.unwrap_or(n))))
    }) else {
        return Ok(HashMap::new());
    };

    let latest = diamond_proxy_l1.provider().get_block_number().await?;
    // With every requested batch committed as of `latest`, each batch's `storedBatchHash` there
    // belongs to its live commit — `load_swept_batches` relies on this for its liveness checks.
    let total_committed = diamond_proxy_l1
        .get_total_batches_committed(latest.into())
        .await?;
    anyhow::ensure!(
        total_committed >= max_batch,
        "batch {max_batch} is not committed on L1 \
         (batches committed as of block {latest}: {total_committed})",
    );
    let (from_block, _) = util::find_l1_commit_block_by_batch_number(diamond_proxy_l1, min_batch)
        .await
        .with_context(|| format!("failed to find live L1 commit block for batch {min_batch}"))?;
    // No live commit of a batch above `min_batch` can precede `min_batch`'s live commit block:
    // it would require a later revert below `min_batch`, which would have reverted that batch as
    // well. So the swept window covers every requested batch.
    sweep_committed_batch_events(diamond_proxy_l1, from_block, max_blocks_per_query).await
}

/// A batch commit collected by [`sweep_committed_batch_events`]: the batch's last
/// `ReportCommittedBatchRangeZKsyncOS` and `BlockCommit` event pair within the swept range.
#[derive(Debug, Clone)]
struct SweptCommit {
    /// Range of L2 blocks that belong to this batch.
    block_range: RangeInclusive<BlockNumber>,
    /// Batch commitment from the `BlockCommit` event.
    commitment: B256,
    /// Transaction that committed the batch.
    tx_hash: TxHash,
    /// Settlement-layer block the commit was observed in.
    l1_block_number: BlockNumber,
}

/// Collects the latest `ReportCommittedBatchRangeZKsyncOS` + `BlockCommit` event pair per batch
/// number in `[from_block, latest]` with a single chunked `eth_getLogs` pass.
///
/// Reverted-and-recommitted batches resolve to their latest commit by keep-last semantics, but a
/// commit that was reverted and never re-committed still lingers in the result — callers must
/// verify liveness (e.g. against `storedBatchHash`) before trusting an entry.
///
/// Runtime is linear in the swept range (one sequential `eth_getLogs` per `max_blocks_per_query`
/// blocks), so this can take minutes when `from_block` lags days behind the L1 tip.
async fn sweep_committed_batch_events(
    zk_chain: &ZkChain<NodeProvider>,
    from_block: BlockNumber,
    max_blocks_per_query: u64,
) -> anyhow::Result<HashMap<u64, SweptCommit>> {
    let provider = zk_chain.provider();
    let latest = provider.get_block_number().await?;
    tracing::info!(from_block, latest, "sweeping L1 for committed batch events");

    // The two events of one commit are folded independently and zipped afterwards; both maps use
    // keep-last semantics so a re-commit within the range overrides the reverted original.
    let mut reports: HashMap<u64, (RangeInclusive<BlockNumber>, TxHash, BlockNumber)> =
        HashMap::new();
    let mut commitments: HashMap<u64, B256> = HashMap::new();

    let mut current_block = from_block;
    while current_block <= latest {
        let filter_to_block = latest.min(current_block + max_blocks_per_query - 1);
        let filter = Filter::new()
            .address(*zk_chain.address())
            .event_signature(vec![
                ReportCommittedBatchRangeZKsyncOS::SIGNATURE_HASH,
                IExecutor::BlockCommit::SIGNATURE_HASH,
            ])
            .from_block(current_block)
            .to_block(filter_to_block);
        let mut logs = provider.get_logs(&filter).await?;
        // `eth_getLogs` results are expected in (block, log index) order already; sort
        // defensively since keep-last semantics depend on it.
        logs.sort_by_key(|log| (log.block_number, log.log_index));
        for log in logs {
            match log.topic0() {
                Some(topic) if *topic == ReportCommittedBatchRangeZKsyncOS::SIGNATURE_HASH => {
                    let report = ReportCommittedBatchRangeZKsyncOS::decode_log(&log.inner)?.data;
                    reports.insert(
                        report.batchNumber,
                        (
                            report.firstBlockNumber..=report.lastBlockNumber,
                            log.transaction_hash.expect("indexed log without tx hash"),
                            log.block_number.expect("indexed log without block number"),
                        ),
                    );
                }
                Some(topic) if *topic == IExecutor::BlockCommit::SIGNATURE_HASH => {
                    let commit = IExecutor::BlockCommit::decode_log(&log.inner)?.data;
                    commitments.insert(commit.batchNumber.to::<u64>(), commit.commitment);
                }
                topic => anyhow::bail!("unexpected event topic in commit sweep: {topic:?}"),
            }
        }
        current_block = filter_to_block + 1;
    }

    Ok(reports
        .into_iter()
        .filter_map(|(batch_number, (block_range, tx_hash, l1_block_number))| {
            let commitment = *commitments.get(&batch_number)?;
            Some((
                batch_number,
                SweptCommit {
                    block_range,
                    commitment,
                    tx_hash,
                    l1_block_number,
                },
            ))
        })
        .collect())
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
/// The prioritized vector contains every batch needed for immediate startup bookkeeping:
/// committed, proved, operational executed, and finalized executed.
fn startup_batch_numbers(
    last_committed_batch: u64,
    last_proved_batch: u64,
    last_executed_batch: u64,
    last_finalized_executed_batch: u64,
) -> (Vec<u64>, Vec<u64>) {
    let prioritized = [
        last_committed_batch,
        last_proved_batch,
        last_executed_batch,
        last_finalized_executed_batch,
    ];
    let (prioritized_in_range, remaining_batch_numbers): (Vec<_>, Vec<_>) =
        (last_finalized_executed_batch.max(1)..=last_committed_batch)
            .partition(|batch_number| prioritized.contains(batch_number));
    (prioritized_in_range, remaining_batch_numbers)
}

/// Resolves the currently live (non-reverted) commit of `batch_number` from L1,
/// returning the discovered batch together with the hash of the transaction that committed it
/// (not to be confused with the batch header hash itself). See
/// [`util::find_l1_commit_block_by_batch_number`] for how liveness is established.
pub async fn fetch_live_committed_batch(
    diamond_proxy: &ZkChain<NodeProvider>,
    batch_number: u64,
) -> anyhow::Result<(DiscoveredCommittedBatch, TxHash)> {
    let (l1_block_with_commit, live_hash) =
        util::find_l1_commit_block_by_batch_number(diamond_proxy, batch_number)
            .await
            .with_context(|| {
                format!("failed to find live L1 commit block for batch {batch_number}")
            })?;

    let (batch, commit_tx_hash) =
        util::fetch_stored_batch_data(diamond_proxy, l1_block_with_commit, batch_number)
            .await?
            .with_context(|| format!("failed to find committed batch {batch_number} on L1"))?;
    // A mismatch means the commit events in `l1_block_with_commit` disagree with
    // `storedBatchHash` (e.g. a revert landed mid-lookup) — fail rather than return stale data.
    anyhow::ensure!(
        batch.batch_info.hash() == live_hash,
        "batch {batch_number} reconstructed from L1 block {l1_block_with_commit} does not hash \
         to the live `storedBatchHash` value {live_hash}",
    );
    Ok((batch, commit_tx_hash))
}

#[cfg(test)]
mod tests {
    use super::startup_batch_numbers;

    #[test]
    fn prioritizes_frontier_batches_once() {
        assert_eq!(startup_batch_numbers(10, 8, 8, 8), (vec![8, 10], vec![9]));
    }

    #[test]
    fn excludes_prioritized_batches_from_remaining_range() {
        assert_eq!(
            startup_batch_numbers(10, 8, 6, 4),
            (vec![4, 6, 8, 10], vec![5, 7, 9])
        );
    }
}
