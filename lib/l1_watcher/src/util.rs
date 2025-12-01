use alloy::primitives::BlockNumber;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Filter;
use alloy::sol_types::SolEvent;
use std::sync::Arc;
use zksync_os_contract_interface::{IExecutor, ZkChain};

pub const ANVIL_L1_CHAIN_ID: u64 = 31337;

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

/// Looks for an L1 batch revert event that happened in block range `[start_block_number; latest_block]`
/// and has affected batch `batch_number`. Returns latest L1 block that contains such an event or `None`
/// if there is not any.
///
/// Assumes no batch commit and revert happened in the same L1 block.
pub async fn find_latest_l1_revert(
    zk_chain: &ZkChain<DynProvider>,
    batch_number: u64,
    start_block_number: BlockNumber,
    max_blocks_to_scan: u64,
) -> anyhow::Result<Option<BlockNumber>> {
    let provider = zk_chain.provider();
    let mut current_block = start_block_number;
    let latest_block = provider.get_block_number().await?;
    tracing::debug!(
        address = %zk_chain.address(),
        start_block_number,
        latest_block,
        max_blocks_to_scan,
        "checking for revert events"
    );

    let mut filter = Filter::new()
        .address(*zk_chain.address())
        .event_signature(IExecutor::BlocksRevert::SIGNATURE_HASH);
    let mut last_block_with_revert = None;
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
            let event = IExecutor::BlocksRevert::decode_log(&log.inner)?.data;
            if event.totalBatchesCommitted < batch_number {
                let l1_block = log
                    .block_number
                    .expect("indexed revert log without block number");
                tracing::info!(
                    %event.totalBatchesCommitted,
                    l1_block,
                    "found batch revert event on L1"
                );
                last_block_with_revert = Some(l1_block)
            }
        }
        current_block = filter_to_block + 1;
    }

    Ok(last_block_with_revert)
}

/// Finds first L1 block that contains **non-reverted** batch commitment event on L1 matching
/// requested batch.
///
/// Returns latest L1 block is there is none.
pub async fn find_l1_commit_block_by_batch_number(
    zk_chain: ZkChain<DynProvider>,
    batch_number: u64,
    max_l1_blocks_to_scan: u64,
) -> anyhow::Result<BlockNumber> {
    let is_batch_committed = move |zk: Arc<ZkChain<DynProvider>>, block: BlockNumber| async move {
        let res = zk.get_total_batches_committed(block.into()).await?;
        Ok(res >= batch_number)
    };
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
        l1_block_with_commit,
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
            // Run binary search one more time but start from `last_l1_block_with_revert` now
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
