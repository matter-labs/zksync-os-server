use crate::watcher::L1WatcherError;
use alloy::consensus::Transaction;
use alloy::primitives::{B256, BlockNumber, Log, TxHash, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context as _;
use backon::{ConstantBuilder, Retryable};
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

/// Finds the settlement-layer block containing the live (non-reverted) commit of `batch_number`,
/// returning it together with the batch's live `storedBatchHash` value.
///
/// The commit block is the block of the **newest** `BlockCommit` event for that batch: under the
/// guard that the batch is currently committed, any earlier commit event was necessarily reverted
/// and superseded (a currently-committed batch's last commit is the one in force), so reverts
/// need no separate handling. The live hash is read from the chain head so callers can
/// cross-check whatever they reconstruct from the commit against it.
///
/// Besides the head reads, discovery runs on `eth_getLogs` alone — deliberately no historical
/// *state* queries, which fail once the chain outgrows a non-archive RPC's state retention (see
/// [`find_last_batch_event_block`]).
///
/// Works for batch 0 too: it is baked into the contract at deployment (no commit event or
/// transaction exists), so the result is the deployment block and its stored hash.
pub(crate) async fn find_l1_commit_block_by_batch_number(
    zk_chain: &ZkChain<NodeProvider>,
    batch_number: u64,
    max_blocks_per_query: u64,
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

    if batch_number == 0 {
        return Ok((zk_chain.deployment_block().await?, live_hash));
    }
    let live_commit_block = find_last_batch_event_block::<IExecutor::BlockCommit>(
        zk_chain,
        batch_number,
        max_blocks_per_query,
    )
    .await?
    .with_context(|| format!("no BlockCommit event found for batch {batch_number}"))?;
    Ok((live_commit_block, live_hash))
}

/// Finds the L1 block containing the execution event of `batch_number`. Execution is
/// never reverted, so the batch has exactly one `BlockExecution` event — found by the
/// same logs-only backward scan as commits (see [`find_last_batch_event_block`]).
pub async fn find_l1_execute_block_by_batch_number(
    zk_chain: &ZkChain<NodeProvider>,
    batch_number: u64,
    max_blocks_per_query: u64,
) -> anyhow::Result<BlockNumber> {
    if batch_number == 0 {
        // Nothing was executed yet; the chain's beginning is the caller's correct
        // continuation point.
        return zk_chain.deployment_block().await;
    }
    find_last_batch_event_block::<IExecutor::BlockExecution>(
        zk_chain,
        batch_number,
        max_blocks_per_query,
    )
    .await?
    .with_context(|| format!("no BlockExecution event found for batch {batch_number}"))
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
    use super::{backward_windows, find_l1_commit_block_by_batch_number};
    use alloy::network::EthereumWallet;
    use alloy::primitives::{Address, B256, Bytes, U64, U256};
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
    /// the chain only through head reads and log queries. The old implementation
    /// binary-searched with `eth_call`s at historical heights, which fail
    /// (`BlockOutOfRangeError`) once the chain outgrows a non-archive RPC's state
    /// retention — killing every node restart on an aged chain. State reads at the
    /// *head* (the committed-count guard and the live stored hash) are fine: head
    /// state is always available. The mock serves responses in exact order, so *any*
    /// other request the discovery makes consumes a mismatched response and fails
    /// the test.
    #[tokio::test]
    async fn commit_discovery_asks_only_for_the_head_and_logs() {
        let asserter = Asserter::new();
        let provider = mocked_provider(&asserter).await;
        let zk_chain = ZkChain::new(Address::repeat_byte(0x11), provider);

        // eth_blockNumber, the two head-state reads (total committed count, live
        // stored batch hash), then the event scan: eth_blockNumber again and one
        // getLogs per backward window until a hit: (151..=250) empty, (51..=150)
        // contains the commit at block 75.
        let live_hash = B256::repeat_byte(0x77);
        asserter.push_success(&U64::from(250));
        asserter.push_success(&Bytes::from(U256::from(9).to_be_bytes::<32>()));
        asserter.push_success(&Bytes::from(live_hash.0));
        asserter.push_success(&U64::from(250));
        asserter.push_success(&Vec::<Log>::new());
        let commit_log: Log = Log {
            block_number: Some(75),
            ..Default::default()
        };
        asserter.push_success(&vec![commit_log]);

        let (block, hash) = find_l1_commit_block_by_batch_number(&zk_chain, 7, 100)
            .await
            .expect("discovery should succeed against the head and logs alone");
        assert_eq!(block, 75);
        assert_eq!(hash, live_hash);
        assert!(
            asserter.read_q().is_empty(),
            "discovery made RPC calls beyond the head reads and log scans"
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
