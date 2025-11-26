use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::consensus::Transaction;
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use alloy::sol_types::{SolCall, SolValue};
use std::sync::Arc;
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::models::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_contract_interface::{IExecutor, ZkChain};

/// Don't try to process that many block linearly
const MAX_L1_BLOCKS_LOOKBEHIND: u64 = 100_000;

/// Discovers block ranges for batches `[last_executed_batch + 1; last_committed_batch]`. This is
/// needed to rebuild batches correctly in Batcher during replay.
pub struct BatchRangeWatcher {
    contract_address: Address,
    provider: DynProvider,
    next_batch_number: u64,
    last_batch_number: u64,
}

impl BatchRangeWatcher {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        last_executed_batch: u64,
        last_committed_batch: u64,
    ) -> anyhow::Result<L1Watcher> {
        let current_l1_block = zk_chain.provider().get_block_number().await?;
        tracing::info!(
            current_l1_block,
            last_executed_batch,
            last_committed_batch,
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 batch range watcher"
        );
        let last_l1_block = find_l1_commit_block_by_batch_number(zk_chain.clone(), last_executed_batch)
            .await
            .or_else(|err| {
                // This may error on Anvil with `--load-state` - as it doesn't support `eth_call` even for recent blocks.
                // We default to `0` in this case - `eth_getLogs` are still supported.
                // Assert that we don't fallback on longer chains (e.g. Sepolia)
                if current_l1_block > MAX_L1_BLOCKS_LOOKBEHIND {
                    anyhow::bail!(
                        "Binary search failed with {err}. Cannot default starting block to zero for a long chain. Current L1 block number: {current_l1_block}. Limit: {MAX_L1_BLOCKS_LOOKBEHIND}."
                    )
                } else {
                    Ok(0)
                }
            })?;
        tracing::info!(last_l1_block, "resolved on L1");

        let this = Self {
            contract_address: *zk_chain.address(),
            provider: zk_chain.provider().clone(),
            next_batch_number: last_executed_batch + 1,
            last_batch_number: last_committed_batch,
        };
        let l1_watcher = L1Watcher::new(
            zk_chain.provider().clone(),
            // We start from last L1 block as it may contain more committed batches apart from the last
            // one.
            last_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            this.into(),
        );

        Ok(l1_watcher)
    }
}

async fn find_l1_commit_block_by_batch_number(
    zk_chain: ZkChain<DynProvider>,
    batch_number: u64,
) -> anyhow::Result<BlockNumber> {
    util::find_l1_block_by_predicate(Arc::new(zk_chain), move |zk, block| async move {
        let res = zk.get_total_batches_committed(block.into()).await?;
        Ok(res >= batch_number)
    })
    .await
}

#[async_trait::async_trait]
impl ProcessL1Event for BatchRangeWatcher {
    const NAME: &'static str = "batch_range";

    type SolEvent = ReportCommittedBatchRangeZKsyncOS;
    type WatchedEvent = ReportCommittedBatchRangeZKsyncOS;

    fn contract_address(&self) -> Address {
        self.contract_address
    }

    async fn process_event(
        &mut self,
        event: ReportCommittedBatchRangeZKsyncOS,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        const V30_ENCODING_VERSION: u8 = 3;

        let batch_number = event.batchNumber;
        let first_block_number = event.firstBlockNumber;
        let last_block_number = event.lastBlockNumber;
        if batch_number < self.next_batch_number {
            tracing::debug!(
                batch_number,
                first_block_number,
                last_block_number,
                "skipping already processed batch range",
            );
        } else if batch_number > self.last_batch_number {
            tracing::trace!(batch_number, "batch is outside of range of interest");
            // todo: provide a way to safely terminate early here (or defer indefinitely)
        } else {
            let tx_hash = log.transaction_hash.expect("indexed log without tx hash");
            // todo: retry-backoff logic in case tx is missing
            let tx = self
                .provider
                .get_transaction_by_hash(tx_hash)
                .await?
                .expect("tx not found");
            let commit_call =
                <IExecutor::commitBatchesSharedBridgeCall as SolCall>::abi_decode(tx.input())?;
            let commit_data = commit_call._commitData;
            if commit_data[0] != V30_ENCODING_VERSION {
                return Err(L1WatcherError::Other(anyhow::anyhow!(
                    "unexpected encoding version: {}",
                    commit_data[0]
                )));
            }

            let (stored_batch_info, mut commit_batch_infos) =
                <(
                    IExecutor::StoredBatchInfo,
                    Vec<IExecutor::CommitBatchInfoZKsyncOS>,
                )>::abi_decode_params(&commit_data[1..])?;
            if commit_batch_infos.len() != 1 {
                return Err(L1WatcherError::Other(anyhow::anyhow!(
                    "unexpected number of committed batch infos: {}",
                    commit_batch_infos.len()
                )));
            }

            let stored_batch_info = StoredBatchInfo::from(stored_batch_info);
            let commit_batch_info = CommitBatchInfo::from(commit_batch_infos.remove(0));

            tracing::info!(
                batch_number,
                first_block_number,
                last_block_number,
                ?stored_batch_info,
                ?commit_batch_info,
                "discovered committed batch range"
            );
        }
        Ok(())
    }
}
