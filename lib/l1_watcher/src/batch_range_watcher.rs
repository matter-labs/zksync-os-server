use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::consensus::Transaction;
use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, BlockNumber};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::Context;
use std::sync::Arc;
use tokio::sync::mpsc;
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::models::CommitBatchInfo;
use zksync_os_contract_interface::{IExecutor, ZkChain};
use zksync_os_types::ProtocolSemanticVersion;

/// Don't try to process that many block linearly
const MAX_L1_BLOCKS_LOOKBEHIND: u64 = 100_000;

/// Discovers commitment data for batches `[last_executed_batch; last_committed_batch]`. This is
/// needed to rebuild batches correctly in Batcher during replay.
pub struct BatchRangeWatcher {
    zk_chain: ZkChain<DynProvider>,
    next_batch_number: u64,
    last_batch_number: u64,
    batch_ranges_sender: mpsc::Sender<CommittedBatch>,
}

impl BatchRangeWatcher {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        last_executed_batch: u64,
        last_committed_batch: u64,
        batch_ranges_sender: mpsc::Sender<CommittedBatch>,
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

        let provider = zk_chain.provider().clone();
        let this = Self {
            zk_chain,
            next_batch_number: last_executed_batch,
            last_batch_number: last_committed_batch,
            batch_ranges_sender,
        };
        let l1_watcher = L1Watcher::new(
            provider,
            // We start from last L1 block as we start watching from `last_executed_batch` inclusively
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
        *self.zk_chain.address()
    }

    fn should_continue(&self) -> bool {
        self.next_batch_number <= self.last_batch_number && self.last_batch_number > 0
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
            // This can trigger if one L1 block has multiple events inside. But generally `Self::should_continue`
            // implementation will stop processor immediately after the last batch of interest was processed.
            tracing::trace!(batch_number, "batch is outside of range of interest");
        } else {
            let tx_hash = log.transaction_hash.expect("indexed log without tx hash");
            // todo: retry-backoff logic in case tx is missing
            let tx = self
                .zk_chain
                .provider()
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

            let (_, mut commit_batch_infos) = <(
                IExecutor::StoredBatchInfo,
                Vec<IExecutor::CommitBatchInfoZKsyncOS>,
            )>::abi_decode_params(&commit_data[1..])?;
            if commit_batch_infos.len() != 1 {
                return Err(L1WatcherError::Other(anyhow::anyhow!(
                    "unexpected number of committed batch infos: {}",
                    commit_batch_infos.len()
                )));
            }

            let commit_batch_info = CommitBatchInfo::from(commit_batch_infos.remove(0));

            if self.next_batch_number != commit_batch_info.batch_number {
                return Err(L1WatcherError::Other(anyhow::anyhow!(
                    "non-sequential batch discovered: expected {}, got {}",
                    self.next_batch_number,
                    commit_batch_info.batch_number
                )));
            }

            tracing::info!(
                batch_number,
                first_block_number,
                last_block_number,
                ?commit_batch_info,
                "discovered committed batch range"
            );

            // L1 block where this batch got committed.
            let block_id =
                BlockId::number(log.block_number.expect("indexed log without block number"));
            // To recreate batch's commitment (and hence it's `StoredBatchInfo` form) we need to
            // know any potential upgrade transaction hash that was applied in this batch.
            //
            // Unfortunately, this information is not passed in `CommitBatchInfo` so we must derive
            // it through other means. Querying `getL2SystemContractsUpgradeTxHash()` and
            // `getL2SystemContractsUpgradeBatchNumber()` should work for the vast majority of cases
            // except when the batch got committed and executed in the same L1 block (which should
            // never happen in current implementation as commit->prove->execute operations are submitted
            // sequentially after at least 1 block confirmation).
            let upgrade_batch_number = self.zk_chain.get_upgrade_batch_number(block_id).await?;
            let upgrade_tx_hash = if upgrade_batch_number == commit_batch_info.batch_number {
                // If the latest upgrade transaction belongs to this batch then current upgrade tx
                // hash must also be present on L1. Thus, we fetch it.
                Some(self.zk_chain.get_upgrade_tx_hash(block_id).await?)
            } else {
                // Either latest in-progress upgrade transaction belongs to a different batch or
                // there is none. If none, `upgrade_batch_number` would be `0` and thus never equal
                // to the currently inspected batch as genesis does not get committed via this flow.
                None
            };
            // Fetch active protocol version at the moment the batch got committed. This should work
            // for the vast majority of cases except when upgrade gets applied in the same L1 block
            // but after batch was committed.
            // todo: validate logic above, maybe it's fine because all batches have to be executed first?
            let packed_protocol_version = self.zk_chain.get_raw_protocol_version(block_id).await?;

            let committed_batch = CommittedBatch {
                commit_info: commit_batch_info,
                upgrade_tx_hash,
                protocol_version: ProtocolSemanticVersion::try_from(packed_protocol_version)
                    .context("invalid protocol version fetched from L1")
                    .map_err(L1WatcherError::Other)?,
            };

            self.batch_ranges_sender
                .send(committed_batch)
                .await
                .map_err(|_| L1WatcherError::OutputClosed)?;
            self.next_batch_number += 1;
        }
        Ok(())
    }
}

/// Commitment information about a batch. Contains enough data to restore `StoredBatchInfo` that
/// got applied on-chain.
#[derive(Debug)]
pub struct CommittedBatch {
    pub commit_info: CommitBatchInfo,
    // todo: this should be a part of `CommitBatchInfo` but needs to be changed on L1 contracts' side first
    pub upgrade_tx_hash: Option<B256>,
    pub protocol_version: ProtocolSemanticVersion,
}
