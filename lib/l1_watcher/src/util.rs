use crate::watcher::L1WatcherError;
use alloy::consensus::Transaction;
use alloy::primitives::{B256, BlockNumber, Log, TxHash, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use backon::{ConstantBuilder, Retryable};
use futures::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;
use zksync_os_batch_types::{CommittedBatchInfo, DiscoveredCommittedBatch};
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::calldata::CommitCalldata;
use zksync_os_contract_interface::models::CommitBatchInfo;
use zksync_os_contract_interface::{IExecutor, ZkChain};
use zksync_os_provider::NodeProvider;

/// Retry policy for data that can transiently lag right after a commit is observed on a
/// load-balanced RPC.
const COMMIT_DATA_RETRY_POLICY: ConstantBuilder = ConstantBuilder::new()
    .with_delay(Duration::from_millis(200))
    .with_max_times(50);

/// Runs `fetch` over `items` with at most `limit` fetches in flight, collecting the keyed
/// results into a map. The first error aborts the whole run.
pub(crate) async fn prefetch_bounded<T, K, V, F, Fut>(
    items: Vec<T>,
    limit: usize,
    fetch: F,
) -> Result<HashMap<K, V>, L1WatcherError>
where
    K: std::hash::Hash + Eq,
    F: Fn(T) -> Fut,
    Fut: Future<Output = Result<(K, V), L1WatcherError>>,
{
    futures::stream::iter(items.into_iter().map(fetch))
        .buffer_unordered(limit.max(1))
        .try_collect()
        .await
}

/// Distance of the first downward gallop probe from the latest block.
///
/// Startup searches almost always target transitions within the last few hundred L1 blocks (the
/// unfinalized frontier), so bracketing from the tip costs O(log(distance-from-tip)) probes at
/// recent — and therefore warm — state instead of O(log(range)) probes at deep-history blocks,
/// which archive RPCs serve much more slowly.
const GALLOP_INITIAL_DISTANCE: u64 = 1_000;

/// Searches `[start_block_number, latest]` for the first block at which `predicate` returns
/// `true`. The predicate must be monotonic over the search range (caller's responsibility).
///
/// Postcondition relied upon by callers: `predicate(result) == true`, and either
/// `predicate(result - 1) == false` or `result == start_block_number`.
///
/// **Caller must ensure `start_block_number >= contract.deployment_block`** — the predicate is
/// invoked without a code-presence guard, so calling it at blocks where the contract is not yet
/// deployed will produce undefined results (typically an RPC error or a `false`-returning revert).
pub async fn find_l1_block_by_predicate<Fut: Future<Output = anyhow::Result<bool>>>(
    provider: &NodeProvider,
    start_block_number: BlockNumber,
    predicate: impl Fn(BlockNumber) -> Fut,
) -> anyhow::Result<BlockNumber> {
    let latest = provider.get_block_number().await?;

    // Ensure the predicate is true by the upper bound, or bail early.
    if !predicate(latest).await? {
        anyhow::bail!(
            "Condition not satisfied up to latest block: contract not deployed yet \
             or target not reached.",
        );
    }

    find_first_true_block(start_block_number, latest, predicate).await
}

/// Core of [`find_l1_block_by_predicate`]: gallop from the tip, then binary-search the bracket.
///
/// Requires `predicate(latest) == true` (checked by the caller). Far-from-tip transitions cost up
/// to ~log2(range / initial distance) extra probes over a plain binary search.
async fn find_first_true_block<Fut: Future<Output = anyhow::Result<bool>>>(
    start_block_number: BlockNumber,
    latest: BlockNumber,
    predicate: impl Fn(BlockNumber) -> Fut,
) -> anyhow::Result<BlockNumber> {
    // Gallop: probe latest-d, latest-2d, latest-4d, ... until the predicate is false or the
    // probe clamps at `start_block_number`.
    let (mut lo, mut hi) = (start_block_number, latest);
    let mut distance = GALLOP_INITIAL_DISTANCE;
    while lo < hi {
        let probe = latest.saturating_sub(distance).max(lo);
        if predicate(probe).await? {
            hi = probe;
            if probe == lo {
                break;
            }
            distance = distance.saturating_mul(2);
        } else {
            lo = probe + 1;
            break;
        }
    }

    // Binary search on [lo, hi] for the first block where the predicate is true.
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if predicate(mid).await? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    Ok(lo)
}

/// Finds the settlement-layer block containing the live (non-reverted) commit of `batch_number`,
/// returning it together with the batch's live `storedBatchHash` value.
///
/// Every commit writes `storedBatchHashes[batch_number] = hash(StoredBatchInfo)`; re-commits
/// overwrite it and reverts do not clear it — hence the guard that the batch is currently
/// committed. Under that guard, `storedBatchHash(batch_number, block) == <live hash>` is a
/// monotonic predicate whose first `true` block is the live commit block, so no `BlocksRevert`
/// scanning is needed.
///
/// Works for batch 0 too: its stored hash is written at contract initialization, so the result
/// is the deployment block (genesis has no commit event or transaction).
pub(crate) async fn find_l1_commit_block_by_batch_number(
    zk_chain: &ZkChain<NodeProvider>,
    batch_number: u64,
) -> anyhow::Result<(BlockNumber, B256)> {
    let latest = zk_chain.provider().get_block_number().await?;
    let total_committed = zk_chain.get_total_batches_committed(latest.into()).await?;
    anyhow::ensure!(
        total_committed >= batch_number,
        "batch {batch_number} is not committed on L1 \
         (batches committed as of block {latest}: {total_committed})",
    );
    let live_hash = zk_chain
        .stored_batch_hash(batch_number, latest.into())
        .await?;

    let deployment_block = zk_chain.deployment_block().await?;
    let live_commit_block = find_l1_block_by_predicate(
        zk_chain.provider(),
        deployment_block,
        move |block| async move {
            Ok(zk_chain
                .stored_batch_hash(batch_number, block.into())
                .await?
                == live_hash)
        },
    )
    .await?;
    Ok((live_commit_block, live_hash))
}

/// Finds first L1 block that contains batch execution event on L1 matching requested batch.
///
/// Returns latest L1 block is there is none.
pub async fn find_l1_execute_block_by_batch_number(
    zk_chain: &ZkChain<NodeProvider>,
    batch_number: u64,
) -> anyhow::Result<BlockNumber> {
    // Execution cannot be reverted, so a plain total-count predicate is safe here, unlike for
    // commits (see `find_l1_commit_block_by_batch_number`).
    let deployment_block = zk_chain.deployment_block().await?;
    find_l1_block_by_predicate(
        zk_chain.provider(),
        deployment_block,
        move |block| async move {
            let res = zk_chain.get_total_batches_executed(block.into()).await?;
            Ok(res >= batch_number)
        },
    )
    .await
}

/// Fetches and decodes stored batch data for batch `batch_number` that is expected to have been
/// committed in `l1_block_number`, returning it together with the commit transaction hash.
/// Returns `None` if requested batch has not been committed in the given L1 block.
pub async fn fetch_stored_batch_data(
    zk_chain: &ZkChain<NodeProvider>,
    l1_block_number: BlockNumber,
    batch_number: u64,
) -> anyhow::Result<Option<(DiscoveredCommittedBatch, TxHash)>> {
    let Some((commit_log, tx_hash)) =
        find_commit_log(zk_chain, l1_block_number, batch_number).await?
    else {
        return Ok(None);
    };
    let batch_info = fetch_committed_batch_data(zk_chain, tx_hash, l1_block_number, batch_number)
        .await?
        .into_stored();

    Ok(Some((
        DiscoveredCommittedBatch {
            batch_info,
            block_range: commit_log.firstBlockNumber..=commit_log.lastBlockNumber,
        },
        tx_hash,
    )))
}

/// Finds the `ReportCommittedBatchRangeZKsyncOS` commit event for `batch_number` in
/// `l1_block_number`, returning the decoded event together with the hash of the transaction that
/// emitted it, or `None` if no matching event is present in that block.
pub(crate) async fn find_commit_log(
    zk_chain: &ZkChain<NodeProvider>,
    l1_block_number: BlockNumber,
    batch_number: u64,
) -> anyhow::Result<Option<(Log<ReportCommittedBatchRangeZKsyncOS>, TxHash)>> {
    let logs = zk_chain
        .provider()
        .get_logs(
            &Filter::new()
                .address(*zk_chain.address())
                .event_signature(ReportCommittedBatchRangeZKsyncOS::SIGNATURE_HASH)
                .from_block(l1_block_number)
                .to_block(l1_block_number),
        )
        .await?;
    // Take the *last* matching log in the block: if the batch was committed, reverted and
    // re-committed within a single L1 block, only the latest commit is the live one.
    Ok(logs
        .into_iter()
        .filter_map(|log| {
            let batch_log = ReportCommittedBatchRangeZKsyncOS::decode_log(&log.inner)
                .expect("unable to decode `ReportCommittedBatchRangeZKsyncOS` log");
            (batch_log.batchNumber == batch_number).then(|| {
                (
                    batch_log,
                    log.transaction_hash.expect("indexed log without tx hash"),
                )
            })
        })
        .next_back())
}

/// Fetches batch commit transaction and extra data from L1 required to construct `CommitedBatch`.
/// Retries if the transaction is pending (exists but has no block number yet) or not yet visible.
pub async fn fetch_committed_batch_data(
    zk_chain: &ZkChain<NodeProvider>,
    tx_hash: TxHash,
    l1_block_number: BlockNumber,
    batch_number: u64,
) -> Result<CommittedBatchInfo, L1WatcherError> {
    // The commit transaction (which carries the `CommitBatchInfo` calldata) and the `BlockCommit`
    // event (which carries the commitment) are independent given the batch number, so we fetch
    // them concurrently. Both can transiently lag right after the commit is observed when hitting
    // a load-balanced RPC, so each is retried.
    let tx_fut = fetch_commit_batch_info(zk_chain, tx_hash, batch_number);

    // The batch commitment is emitted in the `BlockCommit` event (indexed by batch number) of the
    // commit transaction. Reading it from L1 directly is safe and accurate, unlike deriving it from
    // the current protocol version / upgrade transaction hash, which reflect the latest chain state
    // rather than the state at the moment this batch was committed. Filtering on the indexed
    // `batchNumber` topic isolates this batch's event directly (a single commit tx covers a range
    // of L2 blocks, and an L1 block may contain commits for several batches).
    let log_fut = async {
        (|| async {
            zk_chain
                .provider()
                .get_logs(
                    &Filter::new()
                        .address(*zk_chain.address())
                        .event_signature(IExecutor::BlockCommit::SIGNATURE_HASH)
                        .topic1(U256::from(batch_number))
                        .from_block(l1_block_number)
                        .to_block(l1_block_number),
                )
                .await
                .map_err(|e| L1WatcherError::Other(e.into()))?
                .into_iter()
                // Take the *last* event in the block — same-block re-commit handling, see
                // `find_commit_log`.
                .next_back()
                .ok_or_else(|| {
                    L1WatcherError::Other(anyhow::anyhow!(
                        "`BlockCommit` event for batch {batch_number} not found in L1 block {l1_block_number}"
                    ))
                })
        })
        .retry(COMMIT_DATA_RETRY_POLICY)
        .await
    };

    let (commit_batch_info, log) = tokio::try_join!(tx_fut, log_fut)?;

    let commitment = IExecutor::BlockCommit::decode_log(&log.inner)
        .map_err(|e| {
            L1WatcherError::Other(anyhow::anyhow!(
                "failed to decode `BlockCommit` event for batch {batch_number}: {e}"
            ))
        })?
        .commitment;

    Ok(CommittedBatchInfo {
        commit_info: commit_batch_info,
        commitment,
    })
}

/// Fetches the commit transaction of batch `batch_number` and decodes the `CommitBatchInfo` its
/// calldata carries. Retries if the transaction is pending (exists but has no block number yet)
/// or not yet visible.
pub(crate) async fn fetch_commit_batch_info(
    zk_chain: &ZkChain<NodeProvider>,
    tx_hash: TxHash,
    batch_number: u64,
) -> Result<CommitBatchInfo, L1WatcherError> {
    let tx = (|| async {
        let tx = zk_chain
            .provider()
            .get_transaction_by_hash(tx_hash)
            .await
            .map_err(|e| L1WatcherError::Other(e.into()))?
            .ok_or_else(|| {
                L1WatcherError::Other(anyhow::anyhow!("commit tx {tx_hash} not found"))
            })?;
        tx.block_number.ok_or_else(|| {
            L1WatcherError::Other(anyhow::anyhow!(
                "commit tx {tx_hash} has no block number (still pending)"
            ))
        })?;
        Ok::<_, L1WatcherError>(tx)
    })
    .retry(COMMIT_DATA_RETRY_POLICY)
    .await?;

    let CommitCalldata {
        commit_batch_info, ..
    } = CommitCalldata::decode(tx.input()).map_err(L1WatcherError::Other)?;
    if commit_batch_info.batch_number != batch_number {
        return Err(L1WatcherError::Other(anyhow::anyhow!(
            "commit tx {tx_hash} encodes batch {} but batch {batch_number} was expected",
            commit_batch_info.batch_number
        )));
    }
    Ok(commit_batch_info)
}

#[cfg(test)]
mod tests {
    use super::find_first_true_block;
    use super::prefetch_bounded;
    use crate::watcher::L1WatcherError;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn prefetch_bounded_never_exceeds_limit_but_runs_concurrently() {
        const LIMIT: usize = 4;
        let in_flight = AtomicUsize::new(0);
        let max_seen = AtomicUsize::new(0);
        let items: Vec<u64> = (0..16).collect();
        let in_flight = &in_flight;
        let max_seen = &max_seen;
        let map = prefetch_bounded(items, LIMIT, |i| async move {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            max_seen.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok::<_, L1WatcherError>((i, ()))
        })
        .await
        .unwrap();
        assert_eq!(map.len(), 16);
        let max = max_seen.load(Ordering::SeqCst);
        assert!(max <= LIMIT, "in-flight fetches exceeded limit: {max}");
        assert!(max >= 2, "fetches ran serially (max in-flight {max})");
    }

    #[tokio::test]
    async fn prefetch_bounded_is_actually_concurrent_within_limit() {
        // All 8 fetches wait on one barrier: completes only if 8 run at once.
        let barrier = tokio::sync::Barrier::new(8);
        let barrier = &barrier;
        let items: Vec<u64> = (0..8).collect();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            prefetch_bounded(items, 8, |i| async move {
                barrier.wait().await;
                Ok::<_, L1WatcherError>((i, ()))
            }),
        )
        .await
        .expect("deadlocked: fetches did not run concurrently");
        assert_eq!(result.unwrap().len(), 8);
    }

    /// Runs the search against a synthetic monotonic predicate (`block >= first_true`) and
    /// returns the result along with every probed block.
    fn search_with_probes(start: u64, latest: u64, first_true: u64) -> (u64, Vec<u64>) {
        let probes = Mutex::new(Vec::new());
        let result = futures::executor::block_on(find_first_true_block(start, latest, |block| {
            probes.lock().unwrap().push(block);
            async move { Ok(block >= first_true) }
        }))
        .unwrap();
        (result, probes.into_inner().unwrap())
    }

    #[test]
    fn finds_transition_near_tip_with_few_shallow_probes() {
        let (result, probes) = search_with_probes(9_200_000, 11_270_000, 11_269_900);
        assert_eq!(result, 11_269_900);
        // Bracketing from the tip must beat a full-range binary search (~21 probes here) and
        // stay within recent blocks.
        assert!(
            probes.len() <= 15,
            "took {} probes: {probes:?}",
            probes.len()
        );
        assert!(probes.iter().all(|&block| block >= 11_260_000));
    }

    #[test]
    fn finds_deep_transition() {
        let (result, probes) = search_with_probes(0, 11_270_000, 42);
        assert_eq!(result, 42);
        assert!(probes.iter().all(|&block| block <= 11_270_000));
    }

    #[test]
    fn returns_start_when_predicate_true_everywhere() {
        let (result, _) = search_with_probes(9_200_000, 11_270_000, 0);
        assert_eq!(result, 9_200_000);
    }

    #[test]
    fn returns_latest_when_transition_at_latest() {
        let (result, _) = search_with_probes(0, 11_270_000, 11_270_000);
        assert_eq!(result, 11_270_000);
    }

    #[test]
    fn probes_never_go_below_start() {
        let (result, probes) = search_with_probes(11_269_000, 11_270_000, 11_269_500);
        assert_eq!(result, 11_269_500);
        assert!(probes.iter().all(|&block| block >= 11_269_000));
    }

    #[test]
    fn handles_start_equal_to_latest() {
        let (result, probes) = search_with_probes(5, 5, 3);
        assert_eq!(result, 5);
        assert!(probes.is_empty());
    }
}
