use crate::watcher::L1WatcherError;
use alloy::consensus::Transaction;
use alloy::eips::BlockId;
use alloy::primitives::{Address, BlockNumber, TxHash, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use zksync_os_batch_types::{DiscoveredCommittedBatch, ExtendedCommitBatchInfo};
use zksync_os_contract_interface::IChainAssetHandler;
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::calldata::CommitCalldata;
use zksync_os_contract_interface::{Bridgehub, IExecutor, MessageRoot, ZkChain};
use zksync_os_provider::NodeProvider;
use zksync_os_types::ProtocolSemanticVersion;

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
    let latest_migration_number = instance
        .migrationNumber(U256::from(chain_id))
        .block(latest.into())
        .call()
        .await?;
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
            let res = instance
                .migrationNumber(U256::from(chain_id))
                .block(block.into())
                .call()
                .await?;
            Ok(res >= target)
        }
    })
    .await
}

/// Maximum number of L1 blocks that we can scan in a reasonable amount of time.
///
/// Rough calculations: 10min * 10 req/s * 1000 blocks/req = 600 * 10 * 1000 = 6_000_000
const MAX_L1_BLOCKS_TO_SCAN_LINEARLY: u64 = 6_000_000;

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

/// Looks for an L1 event that happened in block range `[start_block_number; latest_block]`
/// and matching provided predicate. Returns latest L1 block that contains such an event or `None`
/// if there is not any.
async fn find_last_matching_event<E: SolEvent + Debug>(
    address: Address,
    provider: &NodeProvider,
    start_block_number: BlockNumber,
    max_blocks_to_scan: u64,
    predicate: impl Fn(&E) -> bool,
) -> anyhow::Result<Option<BlockNumber>> {
    let mut current_block = start_block_number;
    let latest_block = provider.get_block_number().await?;

    tracing::debug!(
        %address,
        start_block_number,
        latest_block,
        max_blocks_to_scan,
        signature = E::SIGNATURE,
        "looking for last matching event"
    );

    // Early return if latest block is behind start block. This can happen if we hit different
    // L1 nodes between calls where the second node is behind the first.
    if latest_block < start_block_number {
        tracing::info!(
            "latest block is behind start block (hitting different L1 nodes?), skipping revert checks"
        );
        return Ok(None);
    }

    let blocks_to_scan = latest_block + 1 - start_block_number;
    if blocks_to_scan > MAX_L1_BLOCKS_TO_SCAN_LINEARLY {
        tracing::warn!(blocks_to_scan, "scanning a lot of L1 blocks");
    }

    let mut filter = Filter::new()
        .address(address)
        .event_signature(E::SIGNATURE_HASH);
    let mut last_block_with_event = None;
    while current_block < latest_block {
        // Inspect up to `max_blocks_to_scan` L1 blocks at a time
        let filter_to_block = latest_block.min(current_block + max_blocks_to_scan - 1);
        filter = filter.from_block(current_block).to_block(filter_to_block);
        let logs = provider.get_logs(&filter).await?;
        tracing::trace!(
            from_block = current_block,
            to_block = filter_to_block,
            log_count = logs.len(),
            "fetched logs"
        );
        for log in logs {
            let event = E::decode_log(&log.inner)?.data;
            if predicate(&event) {
                let l1_block = log
                    .block_number
                    .expect("indexed event log without block number");
                tracing::debug!(
                    %address,
                    ?event,
                    "found new matching event on L1"
                );
                last_block_with_event = Some(l1_block);
            }
        }
        current_block = filter_to_block + 1;
    }
    Ok(last_block_with_event)
}

/// Looks for an L1 batch revert event that happened in block range `[start_block_number; latest_block]`
/// and has affected batch `batch_number`. Returns latest L1 block that contains such an event or `None`
/// if there is not any.
///
/// Batch `batch_number` MUST have been committed before `start_block_number`.
async fn find_latest_l1_revert(
    zk_chain: &ZkChain<NodeProvider>,
    batch_number: u64,
    start_block_number: BlockNumber,
    max_blocks_to_scan: u64,
) -> anyhow::Result<Option<BlockNumber>> {
    find_last_matching_event::<IExecutor::BlocksRevert>(
        *zk_chain.address(),
        zk_chain.provider(),
        start_block_number,
        max_blocks_to_scan,
        |e| e.totalBatchesCommitted < batch_number,
    )
    .await
}

/// Finds first L1 block that contains **non-reverted** batch commitment event on L1 matching
/// requested batch.
///
/// Returns latest L1 block is there is none.
///
/// For any batch `B` that was reverted in tx `T` belonging to L1 block `b` the following MUST hold:
/// `b` CAN contain commit event for `B` that happened either before `T` or after `T` but MUST NOT
/// contain both. See comments inside the implementation for more details.
pub async fn find_l1_commit_block_by_batch_number(
    zk_chain: ZkChain<NodeProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<BlockNumber> {
    let is_batch_committed = move |zk: Arc<ZkChain<NodeProvider>>, block: BlockNumber| async move {
        let res = zk.get_total_batches_committed(block.into()).await?;
        Ok(res >= batch_number)
    };
    let deployment_block = zk_chain.deployment_block().await?;
    // This predicate is not monotonic because committed batches can be reverted. Even then, this
    // binary search will find **some** L1 block that commits our batch. If revert and another commit
    // happen after the found L1 block, then we will find them as handled by logic in the rest of the
    // function. If there are none, then we will not find anything and return this L1 block as a
    // result.
    let l1_block_with_commit = find_l1_block_by_predicate(
        Arc::new(zk_chain.clone()),
        deployment_block,
        is_batch_committed,
    )
    .await?;
    tracing::debug!(
        batch_number,
        l1_block_with_commit,
        "found first L1 block containing batch commitment"
    );

    let last_l1_block_with_revert = find_latest_l1_revert(
        &zk_chain,
        batch_number,
        // Start from next block as current block might contain unrelated reverts. Note that our
        // batch was observed as committed at the END of block `l1_block_with_commit` so any
        // preceding reverts are irrelevant.
        l1_block_with_commit + 1,
        max_l1_blocks_to_scan,
    )
    .await?;
    match last_l1_block_with_revert {
        Some(last_l1_block_with_revert) => {
            tracing::info!(
                batch_number,
                last_l1_block_with_revert,
                "looking for batch commitment after last revert"
            );
            // Run binary search one more time but start from `last_l1_block_with_revert` now.
            // `last_l1_block_with_revert` might contain EITHER commit event for our batch that
            // happened BEFORE revert or AFTER revert. But it cannot contain both, otherwise L1
            // Watcher will index reverted commit first. To mitigate this, we can make L1 Watcher
            // interactively resistant to reverts that happened in the same block (it would watch
            // for both `BlockCommit` and `BlocksRevert`). This scenario should not happen in the
            // current implementation, however, and hence can be safely ignored for now.
            let l1_block_with_commit = find_l1_block_by_predicate(
                Arc::new(zk_chain),
                last_l1_block_with_revert,
                is_batch_committed,
            )
            .await?;
            tracing::info!(
                batch_number,
                l1_block_with_commit,
                "found non-reverted batch commitment on L1"
            );
            Ok(l1_block_with_commit)
        }
        None => {
            tracing::info!(
                batch_number,
                l1_block_with_commit,
                "no batch reverts found on L1"
            );
            Ok(l1_block_with_commit)
        }
    }
}

/// Finds first L1 block that contains batch execution event on L1 matching requested batch.
///
/// Returns latest L1 block is there is none.
pub async fn find_l1_execute_block_by_batch_number(
    zk_chain: ZkChain<NodeProvider>,
    batch_number: u64,
) -> anyhow::Result<BlockNumber> {
    // Execution cannot be reverted, so unlike in `find_l1_commit_block_by_batch_number`, we do not need
    // to take L1 reverts into account here.
    let deployment_block = zk_chain.deployment_block().await?;
    find_l1_block_by_predicate(
        Arc::new(zk_chain),
        deployment_block,
        move |zk, block| async move {
            let res = zk.get_total_batches_executed(block.into()).await?;
            Ok(res >= batch_number)
        },
    )
    .await
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
    let Some((log, tx_hash)) = logs.into_iter().find_map(|log| {
        let batch_log = ReportCommittedBatchRangeZKsyncOS::decode_log(&log.inner)
            .expect("unable to decode `ReportCommittedBatchRangeZKsyncOS` log");
        if batch_log.batchNumber == batch_number {
            Some((
                batch_log,
                log.transaction_hash.expect("indexed log without tx hash"),
            ))
        } else {
            None
        }
    }) else {
        return Ok(None);
    };
    let batch_info = fetch_committed_batch_data(zk_chain, tx_hash, l1_block_number)
        .await?
        .into_stored();

    Ok(Some(DiscoveredCommittedBatch {
        batch_info,
        block_range: log.firstBlockNumber..=log.lastBlockNumber,
    }))
}

/// Fetches batch commit transaction and extra data from L1 required to construct `CommitedBatch`.
/// Retries if the transaction is pending (exists but has no block number yet) or not yet visible.
pub async fn fetch_committed_batch_data(
    zk_chain: &ZkChain<NodeProvider>,
    tx_hash: TxHash,
    l1_block_number: BlockNumber,
) -> Result<ExtendedCommitBatchInfo, L1WatcherError> {
    // L1 block where this batch got committed.
    let l1_block_id = BlockId::number(l1_block_number);
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
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_millis(200))
                .with_max_times(50),
        )
        .await
    };
    let upgrade_fut = async {
        zk_chain
            .get_upgrade_batch_number(l1_block_id)
            .await
            .map_err(|e| L1WatcherError::Other(e.into()))
    };
    // Fetch active protocol version at the moment the batch got committed. This should work
    // for the vast majority of cases except when upgrade gets applied in the same L1 block
    // but after batch was committed.
    let packed_protocol_version_fut = async {
        zk_chain
            .get_raw_protocol_version(l1_block_id)
            .await
            .map_err(|e| L1WatcherError::Other(e.into()))
    };
    let (tx, upgrade_batch_number, packed_protocol_version) =
        tokio::try_join!(tx_fut, upgrade_fut, packed_protocol_version_fut,)?;

    let CommitCalldata {
        commit_batch_info, ..
    } = CommitCalldata::decode(tx.input()).map_err(L1WatcherError::Other)?;

    // To recreate batch's commitment (and hence it's `StoredBatchInfo` form) we need to
    // know any potential upgrade transaction hash that was applied in this batch.
    //
    // Unfortunately, this information is not passed in `CommitBatchInfo` so we must derive
    // it through other means. Querying `getL2SystemContractsUpgradeTxHash()` and
    // `getL2SystemContractsUpgradeBatchNumber()` should work for the vast majority of cases
    // except when the batch got committed and executed in the same L1 block (which should
    // never happen in current implementation as commit->prove->execute operations are submitted
    // sequentially after at least 1 block confirmation).
    let upgrade_tx_hash = if upgrade_batch_number == commit_batch_info.batch_number {
        // If the latest upgrade transaction belongs to this batch then current upgrade tx
        // hash must also be present on L1. Thus, we fetch it.
        Some(zk_chain.get_upgrade_tx_hash(l1_block_id).await?)
    } else {
        // Either latest in-progress upgrade transaction belongs to a different batch or
        // there is none. If none, `upgrade_batch_number` would be `0` and thus never equal
        // to the currently inspected batch as genesis does not get committed via this flow.
        None
    };
    Ok(ExtendedCommitBatchInfo {
        commit_info: commit_batch_info,
        upgrade_tx_hash,
        protocol_version: ProtocolSemanticVersion::try_from(packed_protocol_version)
            .context("invalid protocol version fetched from L1")
            .map_err(L1WatcherError::Other)?,
    })
}
