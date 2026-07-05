use crate::watcher::L1WatcherError;
use alloy::consensus::Transaction;
use alloy::primitives::{Address, BlockNumber, Log, TxHash, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context as _;
use backon::{ConstantBuilder, Retryable};
use std::sync::Arc;
use std::time::Duration;
use zksync_os_batch_types::{CommittedBatchInfo, DiscoveredCommittedBatch};
use zksync_os_contract_interface::IChainAssetHandler;
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::calldata::CommitCalldata;
use zksync_os_contract_interface::is_method_missing;
use zksync_os_contract_interface::{Bridgehub, IExecutor, MessageRoot, ZkChain};
use zksync_os_provider::NodeProvider;

/// Finds the first block where `IChainAssetHandler::migrationNumber(chain_id) >= migration_number`
/// using binary search. Returns latest block if migration number is not reached yet.
///
/// Used by both [`GatewayMigrationWatcher`][crate::GatewayMigrationWatcher] (on L1) and
/// [`MigrationCompleteWatcher`][crate::MigrationCompleteWatcher] (on the current settlement layer)
/// to determine the block from which to start scanning for migration events.
pub async fn find_block_by_migration_number(
    zk_chain: ZkChain<NodeProvider>,
    chain_asset_handler: Address,
    chain_id: u64,
    migration_number: u64,
) -> anyhow::Result<BlockNumber> {
    let instance = Arc::new(IChainAssetHandler::new(
        chain_asset_handler,
        zk_chain.provider().clone(),
    ));
    let target = U256::from(migration_number);
    let latest = instance.provider().get_block_number().await?;
    let latest_migration_number = match instance
        .migrationNumber(U256::from(chain_id))
        .block(latest.into())
        .call()
        .await
    {
        Ok(n) => n,
        // Pre-V31 `ChainAssetHandler` does not expose `migrationNumber`. No Gateway migrations can
        // exist in that era, so there is nothing to scan for — start from the latest block.
        Err(err) if is_method_missing(&err) => return Ok(latest),
        Err(err) => return Err(err.into()),
    };
    // If this migration has not been reached yet, return the latest block.
    if latest_migration_number < migration_number {
        return Ok(latest);
    }

    // The chain's diamond proxy deployment block is a safe lower bound for CAH searches: the proxy
    // can only exist when the bridgehub ecosystem (including CAH, when present) is at least
    // partially up. The predicate still guards against CAH being absent for the V30→V31 migration
    // window where the proxy existed before CAH was deployed.
    let start_block = zk_chain.deployment_block().await?;
    find_l1_block_by_predicate(Arc::new(zk_chain), start_block, move |zk, block| {
        let instance = instance.clone();
        async move {
            let code = zk
                .provider()
                .get_code_at(*instance.address())
                .block_id(block.into())
                .await?;
            if code.0.is_empty() {
                return Ok(false);
            }
            // At this block the address may have code but not yet be the ChainAssetHandler
            // (e.g. a proxy upgraded to it only later), so `migrationNumber` reverts. Treat a
            // revert as "not deployed yet" (false); real RPC errors still propagate.
            let res = match instance
                .migrationNumber(U256::from(chain_id))
                .block(block.into())
                .call()
                .await
            {
                Ok(res) => res,
                Err(err) if is_method_missing(&err) => return Ok(false),
                Err(err) => return Err(err.into()),
            };
            Ok(res >= target)
        }
    })
    .await
}

/// Block windows for scanning `[0, latest]` newest-first: `[latest-chunk+1, latest]`,
/// then the next `chunk` blocks below, ending with a (possibly shorter) window that
/// starts at 0.
pub(crate) fn backward_windows(
    latest: BlockNumber,
    chunk: u64,
) -> impl Iterator<Item = (u64, u64)> {
    let chunk = chunk.max(1);
    let mut high = Some(latest);
    std::iter::from_fn(move || {
        let hi = high?;
        let lo = (hi + 1).saturating_sub(chunk);
        high = lo.checked_sub(1);
        Some((lo, hi))
    })
}

/// Finds the newest event `E` on `zk_chain`'s diamond proxy whose first indexed topic
/// is `batch_number`, scanning `eth_getLogs` backward from the chain head in windows
/// of `max_blocks_per_query`. Returns the L1 block containing it.
///
/// This deliberately never queries historical *state*: log queries are served over an
/// RPC's full history, while `eth_call` at old heights fails once the chain outgrows
/// the node's state retention (~128–256 blocks on non-archive nodes, and similarly on
/// a long-lived anvil) — which would make every node restart on an aged chain fail.
async fn find_last_batch_event_block<E: SolEvent>(
    zk_chain: &ZkChain<NodeProvider>,
    batch_number: u64,
    max_blocks_per_query: u64,
) -> anyhow::Result<Option<BlockNumber>> {
    let latest = zk_chain.provider().get_block_number().await?;
    for (from, to) in backward_windows(latest, max_blocks_per_query) {
        let logs = zk_chain
            .provider()
            .get_logs(
                &Filter::new()
                    .address(*zk_chain.address())
                    .event_signature(E::SIGNATURE_HASH)
                    .topic1(U256::from(batch_number))
                    .from_block(from)
                    .to_block(to),
            )
            .await?;
        // Logs come ordered oldest-first; the newest one in the newest non-empty
        // window is the newest match overall.
        if let Some(log) = logs.last() {
            return Ok(Some(
                log.block_number
                    .expect("indexed event log without block number"),
            ));
        }
    }
    Ok(None)
}

/// Binary-searches `[start_block_number, latest]` for the first block at which `predicate` returns
/// `true`. The predicate must be monotonic over the search range (caller's responsibility).
///
/// **Caller must ensure `start_block_number >= contract.deployment_block`** — the predicate is
/// invoked without a code-presence guard, so calling it at blocks where the contract is not yet
/// deployed will produce undefined results (typically an RPC error or a `false`-returning revert).
pub async fn find_l1_block_by_predicate<Fut: Future<Output = anyhow::Result<bool>>>(
    zk_chain: Arc<ZkChain<NodeProvider>>,
    start_block_number: BlockNumber,
    predicate: impl Fn(Arc<ZkChain<NodeProvider>>, u64) -> Fut,
) -> anyhow::Result<BlockNumber> {
    let latest = zk_chain.provider().get_block_number().await?;

    // Ensure the predicate is true by the upper bound, or bail early.
    if !predicate(zk_chain.clone(), latest).await? {
        anyhow::bail!(
            "Condition not satisfied up to latest block: contract not deployed yet \
             or target not reached.",
        );
    }

    // Binary search on [start_block_number, latest] for the first block where predicate is true.
    let (mut lo, mut hi) = (start_block_number, latest);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if predicate(zk_chain.clone(), mid).await? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    Ok(lo)
}

/// Finds the L1 block containing the **currently valid** commit of `batch_number`:
/// the newest `BlockCommit` event for that batch. Any earlier commit event was
/// necessarily reverted and superseded (a currently-committed batch's last commit is
/// the one in force), so reverts need no separate handling.
///
/// The caller is responsible for only asking about batches that are committed at the
/// chain head (all callers read them from the head L1 state); for an uncommitted
/// batch the newest event — if any — would be a stale, reverted commit.
pub async fn find_l1_commit_block_by_batch_number(
    zk_chain: ZkChain<NodeProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<BlockNumber> {
    if batch_number == 0 {
        // The genesis batch is baked into the contract at deployment; no commit
        // event exists for it. Callers special-case genesis before searching, so
        // this is only a defensive floor.
        return Ok(0);
    }
    find_last_batch_event_block::<IExecutor::BlockCommit>(
        &zk_chain,
        batch_number,
        max_l1_blocks_to_scan,
    )
    .await?
    .with_context(|| format!("no BlockCommit event found for batch {batch_number}"))
}

/// Finds the L1 block containing the execution event of `batch_number`. Execution is
/// never reverted, so the batch has exactly one `BlockExecution` event.
pub async fn find_l1_execute_block_by_batch_number(
    zk_chain: ZkChain<NodeProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<BlockNumber> {
    if batch_number == 0 {
        // Nothing was executed yet; scanning "from genesis onwards" is the caller's
        // correct continuation point.
        return Ok(0);
    }
    find_last_batch_event_block::<IExecutor::BlockExecution>(
        &zk_chain,
        batch_number,
        max_l1_blocks_to_scan,
    )
    .await?
    .with_context(|| format!("no BlockExecution event found for batch {batch_number}"))
}

/// Finds the first L1 block where `interopRootLogId >= next_interop_root_id`.
/// Uses binary search for efficiency.
pub async fn find_l1_block_by_interop_root_id(
    bridgehub: Bridgehub<NodeProvider>,
    next_interop_root_id: u64,
) -> anyhow::Result<BlockNumber> {
    if next_interop_root_id == 0 {
        return Ok(0);
    }

    let message_root_address = bridgehub.message_root_address().await?;
    let message_root = Arc::new(MessageRoot::new(
        message_root_address,
        bridgehub.provider().clone(),
    ));

    let latest = message_root.provider().get_block_number().await?;
    // The provider's cache resolves (and remembers) the MessageRoot deployment block, giving the
    // search a tight lower bound without a per-iteration code-existence guard.
    let deployment_block = message_root.deployment_block().await?;

    let predicate =
        async |message_root: Arc<MessageRoot<NodeProvider>>, block: u64| -> anyhow::Result<bool> {
            let res = message_root.interop_root_log_id(block.into()).await?;
            Ok(res >= next_interop_root_id)
        };

    if !predicate(message_root.clone(), latest).await? {
        anyhow::bail!(
            "Condition not satisfied up to latest block: contract not deployed yet \
             or target not reached.",
        );
    }

    let (mut lo, mut hi) = (deployment_block, latest);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if predicate(message_root.clone(), mid).await? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    Ok(lo)
}

/// Fetches and decodes stored batch data for batch `batch_number` that is expected to have been
/// committed in `l1_block_number`. Returns `None` if requested batch has not been committed in
/// the given L1 block.
pub async fn fetch_stored_batch_data(
    zk_chain: &ZkChain<NodeProvider>,
    l1_block_number: BlockNumber,
    batch_number: u64,
) -> anyhow::Result<Option<DiscoveredCommittedBatch>> {
    let Some((commit_log, tx_hash)) =
        find_commit_log(zk_chain, l1_block_number, batch_number).await?
    else {
        return Ok(None);
    };
    let batch_info = fetch_committed_batch_data(zk_chain, tx_hash, l1_block_number, batch_number)
        .await?
        .into_stored();

    Ok(Some(DiscoveredCommittedBatch {
        batch_info,
        block_range: commit_log.firstBlockNumber..=commit_log.lastBlockNumber,
    }))
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
    Ok(logs.into_iter().find_map(|log| {
        let batch_log = ReportCommittedBatchRangeZKsyncOS::decode_log(&log.inner)
            .expect("unable to decode `ReportCommittedBatchRangeZKsyncOS` log");
        (batch_log.batchNumber == batch_number).then(|| {
            (
                batch_log,
                log.transaction_hash.expect("indexed log without tx hash"),
            )
        })
    }))
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
    let retry_policy = || {
        ConstantBuilder::default()
            .with_delay(Duration::from_millis(200))
            .with_max_times(50)
    };

    let tx_fut = async {
        (|| async {
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
        .retry(retry_policy())
        .await
    };

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
                .next()
                .ok_or_else(|| {
                    L1WatcherError::Other(anyhow::anyhow!(
                        "`BlockCommit` event for batch {batch_number} not found in L1 block {l1_block_number}"
                    ))
                })
        })
        .retry(retry_policy())
        .await
    };

    let (tx, log) = tokio::try_join!(tx_fut, log_fut)?;

    let CommitCalldata {
        commit_batch_info, ..
    } = CommitCalldata::decode(tx.input()).map_err(L1WatcherError::Other)?;
    if commit_batch_info.batch_number != batch_number {
        return Err(L1WatcherError::Other(anyhow::anyhow!(
            "commit tx {tx_hash} encodes batch {} but batch {batch_number} was expected",
            commit_batch_info.batch_number
        )));
    }

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

#[cfg(test)]
mod tests {
    use super::{backward_windows, find_l1_commit_block_by_batch_number};
    use alloy::network::EthereumWallet;
    use alloy::primitives::{Address, U64};
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::types::{Block, Header, Log};
    use alloy::transports::mock::Asserter;
    use zksync_os_contract_interface::ZkChain;
    use zksync_os_provider::NodeProvider;

    fn header_with_number(number: u64) -> Header {
        let mut block: Block = Block::default();
        block.header.inner.number = number;
        block.header
    }

    async fn mocked_provider(asserter: &Asserter) -> NodeProvider {
        // `NodeProvider` capability probes consume three responses:
        // get_header(latest), get_header(finalized), chain id.
        asserter.push_success(&header_with_number(250));
        asserter.push_success(&header_with_number(250));
        asserter.push_success(&U64::from(1));
        NodeProvider::new(
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .wallet(EthereumWallet::default())
                .connect_mocked_client(asserter.clone()),
        )
        .await
        .expect("mocked provider construction should succeed")
    }

    /// The regression pin for the chaos rig's finding #2: batch discovery must reach
    /// the chain only through the head number and log queries. The old
    /// implementation binary-searched with `eth_call`s at historical heights, which
    /// fail (`BlockOutOfRangeError`) once the chain outgrows a non-archive RPC's
    /// state retention — killing every node restart on an aged chain. The mock
    /// serves responses in exact order, so *any* other request the discovery makes
    /// consumes a mismatched response and fails the test.
    #[tokio::test]
    async fn commit_discovery_asks_only_for_the_head_and_logs() {
        let asserter = Asserter::new();
        let provider = mocked_provider(&asserter).await;
        let zk_chain = ZkChain::new(Address::repeat_byte(0x11), provider);

        // eth_blockNumber, then one getLogs per backward window until a hit:
        // (151..=250) empty, (51..=150) contains the commit at block 75.
        asserter.push_success(&U64::from(250));
        asserter.push_success(&Vec::<Log>::new());
        let commit_log: Log = Log {
            block_number: Some(75),
            ..Default::default()
        };
        asserter.push_success(&vec![commit_log]);

        let block = find_l1_commit_block_by_batch_number(zk_chain, 7, 100)
            .await
            .expect("discovery should succeed against logs alone");
        assert_eq!(block, 75);
        assert!(
            asserter.read_q().is_empty(),
            "discovery made RPC calls beyond the head number and log scans"
        );
    }

    #[test]
    fn windows_cover_the_whole_range_newest_first_without_overlap() {
        let windows: Vec<_> = backward_windows(10, 4).collect();
        assert_eq!(windows, vec![(7, 10), (3, 6), (0, 2)]);
    }

    #[test]
    fn window_edges() {
        // Chunk exactly divides the range.
        assert_eq!(
            backward_windows(9, 5).collect::<Vec<_>>(),
            vec![(5, 9), (0, 4)]
        );
        // Single-block chain.
        assert_eq!(backward_windows(0, 100).collect::<Vec<_>>(), vec![(0, 0)]);
        // Chunk larger than the chain.
        assert_eq!(backward_windows(3, 100).collect::<Vec<_>>(), vec![(0, 3)]);
        // A zero chunk must not loop forever.
        assert_eq!(
            backward_windows(2, 0).collect::<Vec<_>>(),
            vec![(2, 2), (1, 1), (0, 0)]
        );
    }
}
