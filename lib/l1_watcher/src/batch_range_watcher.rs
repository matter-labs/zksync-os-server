use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::consensus::Transaction;
use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, BlockNumber, TxHash};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use anyhow::Context;
use tokio::sync::mpsc;
use zksync_os_batch_types::BatchInfo;
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::calldata::CommitCalldata;
use zksync_os_contract_interface::models::{CommitBatchInfo, StoredBatchInfo};
use zksync_os_types::ProtocolSemanticVersion;

/// Discovers commitment data for batches `[last_executed_batch; last_committed_batch]`. This is
/// needed to rebuild batches correctly in Batcher during replay.
pub struct BatchRangeWatcher {
    zk_chain: ZkChain<DynProvider>,
    /// Next batch expected to be observed as committed on L1 (along with its block range). This
    /// starts at `last_executed_batch` and progresses all the way to `last_committed_batch` (both
    /// as observed on startup).
    next_batch_number: u64,
    /// Last committed batch as was observed on startup. When `next_batch_number` becomes greater
    /// than this value, `BatchRangeWatcher` is considered to have finished executing.
    last_committed_batch_on_startup: u64,
    /// Commitment information for batches `[last_executed_batch; last_committed_batch]` is sent
    /// to this channel. Once `BatchRangeWatcher` finishes running (all batches have been sent),
    /// this end of the channel will be closed.
    batch_ranges_sender: mpsc::Sender<CommittedBatch>,
}

/// Initialization data returned by `BatchRangeWatcher`'s constructor.
pub struct BatchRangeWatcherInit {
    /// L1 watcher instance that is expected to be run by main process.
    pub l1_watcher: L1Watcher,
    /// Data about last executed batch as fetched and decoded from L1. `None` iff last executed
    /// batch is genesis (there are no execution events on L1 yet).
    // todo: consider moving logic to l1-discovery: fetch and decode last committed/proven/executed
    //       batches on startup
    pub last_executed_batch_data: Option<StoredBatchData>,
}

impl BatchRangeWatcher {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        last_executed_batch: u64,
        last_committed_batch: u64,
        batch_ranges_sender: mpsc::Sender<CommittedBatch>,
    ) -> anyhow::Result<BatchRangeWatcherInit> {
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
        let last_l1_block = util::find_l1_commit_block_by_batch_number(
            zk_chain.clone(),
            last_executed_batch,
            config.max_blocks_to_process,
        )
        .await?;
        tracing::info!(last_l1_block, "resolved on L1");

        let provider = zk_chain.provider().clone();
        let this = Self {
            zk_chain,
            next_batch_number: last_executed_batch + 1,
            last_committed_batch_on_startup: last_committed_batch,
            batch_ranges_sender,
        };
        let last_executed_batch_data = this
            .fetch_stored_batch_data(last_l1_block, last_executed_batch)
            .await?;

        let l1_watcher = L1Watcher::new(
            provider,
            // We start from last L1 block as it may contain more committed batches apart from the last
            // one.
            last_l1_block,
            config.max_blocks_to_process,
            config.poll_interval,
            this.into(),
        );

        Ok(BatchRangeWatcherInit {
            l1_watcher,
            last_executed_batch_data,
        })
    }

    /// Fetches and decodes batch commit transaction. Fails if transaction does not exist or is not
    /// a valid commit transaction.
    async fn fetch_commit_calldata(
        &self,
        tx_hash: TxHash,
    ) -> Result<CommittedBatch, L1WatcherError> {
        // todo: retry-backoff logic in case tx is missing
        let tx = self
            .zk_chain
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
        CommittedBatch::fetch(&self.zk_chain, commit_batch_info, l1_block_id).await
    }

    /// Fetches and decodes stored batch data for batch `batch_number` that is expected to have been
    /// committed in `l1_block_number`. Returns `None` if requested batch has not been committed in
    /// the given L1 block.
    async fn fetch_stored_batch_data(
        &self,
        l1_block_number: BlockNumber,
        batch_number: u64,
    ) -> anyhow::Result<Option<StoredBatchData>> {
        let logs = self
            .zk_chain
            .provider()
            .get_logs(
                &Filter::new()
                    .address(*self.zk_chain.address())
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
        let committed_batch = self.fetch_commit_calldata(tx_hash).await?;

        // todo: stop using this struct once fully migrated from S3
        let last_executed_batch_info = BatchInfo {
            commit_info: committed_batch.commit_info,
            chain_address: Default::default(),
            upgrade_tx_hash: committed_batch.upgrade_tx_hash,
            blob_sidecar: None,
        };
        let batch_info = last_executed_batch_info.into_stored(&committed_batch.protocol_version);

        Ok(Some(StoredBatchData {
            batch_info,
            first_block_number: log.firstBlockNumber,
            last_block_number: log.lastBlockNumber,
        }))
    }
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
        // Watch events up to last committed batch. Finish early if it's genesis batch as there will
        // be no event.
        self.next_batch_number <= self.last_committed_batch_on_startup
            && self.last_committed_batch_on_startup > 0
    }

    async fn process_event(
        &mut self,
        event: ReportCommittedBatchRangeZKsyncOS,
        log: Log,
    ) -> Result<(), L1WatcherError> {
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
        } else if batch_number > self.last_committed_batch_on_startup {
            // This can trigger if one L1 block has multiple events inside. But generally `Self::should_continue`
            // implementation will stop processor immediately after the last batch of interest was processed.
            tracing::trace!(batch_number, "batch is outside of range of interest");
        } else {
            let tx_hash = log.transaction_hash.expect("indexed log without tx hash");
            let committed_batch = self.fetch_commit_calldata(tx_hash).await?;

            if self.next_batch_number != committed_batch.commit_info.batch_number {
                return Err(L1WatcherError::Other(anyhow::anyhow!(
                    "non-sequential batch discovered: expected {}, got {}",
                    self.next_batch_number,
                    committed_batch.commit_info.batch_number
                )));
            }

            tracing::info!(
                batch_number,
                first_block_number,
                last_block_number,
                ?committed_batch,
                "discovered committed batch range"
            );

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
    // todo: this should be a part of `CommitBatchInfo` but needs to be changed on L1 contracts' side first
    pub protocol_version: ProtocolSemanticVersion,
}

impl CommittedBatch {
    /// Fetches extra information that is not available inside `CommitBatchInfo` from L1 to construct
    /// `CommitedBatch`. Requires `l1_block_id` where the batch was committed.
    async fn fetch(
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

/// Information about a stored batch on L1. Compared to plain `StoredBatchInfo` also contains block
/// range belonging to this batch.
pub struct StoredBatchData {
    pub batch_info: StoredBatchInfo,
    pub first_block_number: BlockNumber,
    pub last_block_number: BlockNumber,
}
