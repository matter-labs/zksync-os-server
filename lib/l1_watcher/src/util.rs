use crate::watcher::L1WatcherError;
use alloy::consensus::Transaction;
use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, BlockNumber, TxHash};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use anyhow::Context;
use std::fmt::Debug;
use std::sync::Arc;
use zksync_os_batch_types::{BatchInfo, DiscoveredCommittedBatch};
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::calldata::CommitCalldata;
use zksync_os_contract_interface::models::CommitBatchInfo;
use zksync_os_contract_interface::{IExecutor, ZkChain};
use zksync_os_types::ProtocolSemanticVersion;

pub const ANVIL_L1_CHAIN_ID: u64 = 31337;

/// Maximum number of L1 blocks that we can scan in a reasonable amount of time.
///
/// Rough calculations: 10min * 10 req/s * 1000 blocks/req = 600 * 10 * 1000 = 6_000_000
const MAX_L1_BLOCKS_TO_SCAN_LINEARLY: u64 = 6_000_000;

pub async fn find_l1_block_by_predicate<Fut: Future<Output = anyhow::Result<bool>>>(
    zk_chain: Arc<ZkChain<DynProvider>>,
    start_block_number: BlockNumber,
    predicate: impl Fn(Arc<ZkChain<DynProvider>>, u64) -> Fut,
) -> anyhow::Result<BlockNumber> {
    if zk_chain.provider().get_chain_id().await? == ANVIL_L1_CHAIN_ID {
        // Binary search may error on Anvil with `--load-state` - as it doesn't support `eth_call`
        // even for recent blocks. We default to `start_block_number` in this case - `eth_getLogs`
        // are still supported.
        return Ok(start_block_number);
    }

    let latest = zk_chain.provider().get_block_number().await?;

    let guarded_predicate =
        async |zk: Arc<ZkChain<DynProvider>>, block: u64| -> anyhow::Result<bool> {
            if !zk.code_exists_at_block(block.into()).await? {
                // return early if contract is not deployed yet - otherwise `predicate` might fail
                return Ok(false);
            }
            predicate(zk, block).await
        };

    // Ensure the predicate is true by the upper bound, or bail early.
    if !guarded_predicate(zk_chain.clone(), latest).await? {
        anyhow::bail!(
            "Condition not satisfied up to latest block: contract not deployed yet \
             or target not reached.",
        );
    }

    // Binary search on [0, latest] for the first block where predicate is true.
    let (mut lo, mut hi) = (start_block_number, latest);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if guarded_predicate(zk_chain.clone(), mid).await? {
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
    provider: &DynProvider,
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
    zk_chain: &ZkChain<DynProvider>,
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
    zk_chain: ZkChain<DynProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<BlockNumber> {
    if zk_chain.provider().get_chain_id().await? == ANVIL_L1_CHAIN_ID {
        // Binary search may error on Anvil with `--load-state` - as it doesn't support `eth_call`
        // for historical blocks. We run linear search as a fallback.
        if batch_number == 0 {
            // For genesis we must return L1 block where `zk_chain` got deployed. For Anvil it's okay
            // to return 0 here as the chain should not be long anyway.
            return Ok(0);
        }
        return find_last_matching_event::<ReportCommittedBatchRangeZKsyncOS>(
            *zk_chain.address(),
            zk_chain.provider(),
            0,
            max_l1_blocks_to_scan,
            |e| e.batchNumber == batch_number,
        )
        .await?
        .with_context(|| {
            format!("linear search failed to find where batch {batch_number} was committed")
        });
    }

    let is_batch_committed = move |zk: Arc<ZkChain<DynProvider>>, block: BlockNumber| async move {
        let res = zk.get_total_batches_committed(block.into()).await?;
        Ok(res >= batch_number)
    };
    // This predicate is not monotonic because committed batches can be reverted. Even then, this
    // binary search will find **some** L1 block that commits our batch. If revert and another commit
    // happen after the found L1 block, then we will find them as handled by logic in the rest of the
    // function. If there are none, then we will not find anything and return this L1 block as a
    // result.
    let l1_block_with_commit =
        find_l1_block_by_predicate(Arc::new(zk_chain.clone()), 0, is_batch_committed).await?;
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
    zk_chain: ZkChain<DynProvider>,
    batch_number: u64,
) -> anyhow::Result<BlockNumber> {
    // Execution cannot be reverted, so unlike in `find_l1_commit_block_by_batch_number`, we do not need
    // to take L1 reverts into account here.
    find_l1_block_by_predicate(Arc::new(zk_chain), 0, move |zk, block| async move {
        let res = zk.get_total_batches_executed(block.into()).await?;
        Ok(res >= batch_number)
    })
    .await
}

/// Fetches and decodes stored batch data for batch `batch_number` that is expected to have been
/// committed in `l1_block_number`. Returns `None` if requested batch has not been committed in
/// the given L1 block.
pub async fn fetch_stored_batch_data(
    zk_chain: &ZkChain<DynProvider>,
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
    let committed_batch = fetch_commit_calldata(zk_chain, tx_hash).await?;

    // todo: stop using this struct once fully migrated from S3
    let last_executed_batch_info = BatchInfo {
        commit_info: committed_batch.commit_info,
        chain_address: Default::default(),
        upgrade_tx_hash: committed_batch.upgrade_tx_hash,
        blob_sidecar: None,
    };
    let batch_info = last_executed_batch_info.into_stored(&committed_batch.protocol_version);

    Ok(Some(DiscoveredCommittedBatch {
        batch_info,
        block_range: log.firstBlockNumber..=log.lastBlockNumber,
    }))
}

/// Finds and decodes stored batch data for batch `batch_number`. Returns `None` if there is none.
pub async fn find_stored_batch_data_by_batch_number(
    zk_chain: &ZkChain<DynProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<Option<DiscoveredCommittedBatch>> {
    let l1_block_with_commit =
        find_l1_commit_block_by_batch_number(zk_chain.clone(), batch_number, max_l1_blocks_to_scan)
            .await?;
    fetch_stored_batch_data(zk_chain, l1_block_with_commit, batch_number).await
}

/// Commitment information about a batch. Contains enough data to restore `StoredBatchInfo` that
/// got applied on-chain.
#[derive(Debug)]
pub struct CommittedBatch {
    pub commit_info: CommitBatchInfo,
    // todo: this should be a part of `CommitBatchInfo` but needs to be changed on L1 contracts' side first
    pub upgrade_tx_hash: Option<B256>,
    // todo: this should be a part of `CommitBatchInfo` but needs to be changed on L1 contracts' side first
    pub protocol_version: ProtocolSemanticVersion,
}

impl CommittedBatch {
    /// Fetches extra information that is not available inside `CommitBatchInfo` from L1 to construct
    /// `CommitedBatch`. Requires `l1_block_id` where the batch was committed.
    pub async fn fetch(
        zk_chain: &ZkChain<DynProvider>,
        commit_batch_info: CommitBatchInfo,
        l1_block_id: BlockId,
    ) -> Result<Self, L1WatcherError> {
        // To recreate batch's commitment (and hence it's `StoredBatchInfo` form) we need to
        // know any potential upgrade transaction hash that was applied in this batch.
        //
        // Unfortunately, this information is not passed in `CommitBatchInfo` so we must derive
        // it through other means. Querying `getL2SystemContractsUpgradeTxHash()` and
        // `getL2SystemContractsUpgradeBatchNumber()` should work for the vast majority of cases
        // except when the batch got committed and executed in the same L1 block (which should
        // never happen in current implementation as commit->prove->execute operations are submitted
        // sequentially after at least 1 block confirmation).
        let upgrade_batch_number = zk_chain.get_upgrade_batch_number(l1_block_id).await?;
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
        // Fetch active protocol version at the moment the batch got committed. This should work
        // for the vast majority of cases except when upgrade gets applied in the same L1 block
        // but after batch was committed.
        let packed_protocol_version = zk_chain.get_raw_protocol_version(l1_block_id).await?;

        Ok(Self {
            commit_info: commit_batch_info,
            upgrade_tx_hash,
            protocol_version: ProtocolSemanticVersion::try_from(packed_protocol_version)
                .context("invalid protocol version fetched from L1")
                .map_err(L1WatcherError::Other)?,
        })
    }
}

/// Fetches and decodes batch commit transaction. Fails if transaction does not exist or is not
/// a valid commit transaction.
pub async fn fetch_commit_calldata(
    zk_chain: &ZkChain<DynProvider>,
    tx_hash: TxHash,
) -> Result<CommittedBatch, L1WatcherError> {
    // todo: retry-backoff logic in case tx is missing
    let tx = zk_chain
        .provider()
        .get_transaction_by_hash(tx_hash)
        .await?
        .expect("tx not found");
    let CommitCalldata {
        commit_batch_info, ..
    } = CommitCalldata::decode(tx.input()).map_err(L1WatcherError::Other)?;

    // L1 block where this batch got committed.
    let l1_block_id = BlockId::number(
        tx.block_number
            .expect("mined transaction has no block number"),
    );
    CommittedBatch::fetch(zk_chain, commit_batch_info, l1_block_id).await
}
