use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use zksync_os_batch_types::{BatchInfo, DiscoveredCommittedBatch};
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::ZkChain;
use zksync_os_storage_api::{WriteBatch, WriteFinality};

/// Persists executed batches via [`WriteBatch`].
/// Note: batches are discovered by `commit_watcher.rs` from L1 as soon as they are committed.
/// However, `commit_watcher.rs` only saves them **in memory** (via `committed_batch_provider.rs`).
/// Only when batch is also executed on L1, this logic kicks in and batches are **persisted on disc**.
/// Committed batches can be rolled back on L1, which is not the case for executed - so this separation
/// ensures that we don't need to rollback any persistent node state on L1 commit rollback.
pub struct L1PersistBatchWatcher<BatchStorage, Finality> {
    zk_chain: ZkChain<DynProvider>,
    batch_storage: BatchStorage,
    finality: Finality,
}

impl<BatchStorage: WriteBatch, Finality: WriteFinality>
    L1PersistBatchWatcher<BatchStorage, Finality>
{
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        batch_storage: BatchStorage,
        finality: Finality,
    ) -> anyhow::Result<L1Watcher> {
        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let last_persisted_batch = batch_storage.latest_batch();
        tracing::info!(
            current_l1_block,
            last_persisted_batch,
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 persist batch watcher"
        );
        let last_l1_block = util::find_l1_commit_block_by_batch_number(
            zk_chain.clone(),
            last_persisted_batch,
            config.max_blocks_to_process,
        )
        .await?;
        tracing::info!(last_l1_block, "resolved on L1");

        let this = Self {
            zk_chain: zk_chain.clone(),
            batch_storage,
            finality,
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

    async fn parse_committed_batch(
        &self,
        report: ReportCommittedBatchRangeZKsyncOS,
        log: Log,
    ) -> Result<DiscoveredCommittedBatch, L1WatcherError> {
        let tx_hash = log.transaction_hash.expect("indexed log without tx hash");
        let committed_batch = util::fetch_commit_calldata(&self.zk_chain, tx_hash).await?;

        // todo: stop using this struct once fully migrated from S3
        let last_executed_batch_info = BatchInfo {
            commit_info: committed_batch.commit_info,
            chain_address: Default::default(),
            upgrade_tx_hash: committed_batch.upgrade_tx_hash,
            blob_sidecar: None,
        };
        let batch_info = last_executed_batch_info.into_stored(&committed_batch.protocol_version);
        Ok(DiscoveredCommittedBatch {
            batch_info,
            block_range: report.firstBlockNumber..=report.lastBlockNumber,
        })
    }
}

#[async_trait::async_trait]
impl<BatchStorage: WriteBatch, Finality: WriteFinality> ProcessL1Event
    for L1PersistBatchWatcher<BatchStorage, Finality>
{
    const NAME: &'static str = "persist_batch";

    type SolEvent = ReportCommittedBatchRangeZKsyncOS;
    type WatchedEvent = ReportCommittedBatchRangeZKsyncOS;

    fn contract_address(&self) -> Address {
        *self.zk_chain.address()
    }

    async fn process_event(
        &mut self,
        report: ReportCommittedBatchRangeZKsyncOS,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let batch_number = report.batchNumber;
        let latest_persisted_batch = self.batch_storage.latest_batch();
        if batch_number <= latest_persisted_batch {
            tracing::debug!(
                batch_number,
                "discovered already persisted batch, validating"
            );
            let committed_batch = self.parse_committed_batch(report, log).await?;
            let stored_batch = self
                .batch_storage
                .get_batch_by_number(batch_number)
                .map_err(L1WatcherError::Other)?
                .expect("persisted batch not found in DB");
            if stored_batch != committed_batch {
                tracing::error!(
                    ?stored_batch,
                    ?committed_batch,
                    batch_number,
                    "discovered batch does not match stored batch"
                );
                return Err(L1WatcherError::Other(anyhow::anyhow!(
                    "discovered batch #{batch_number} does not match stored batch"
                )));
            }
        } else if batch_number > latest_persisted_batch + 1 {
            // This should only be possible if we skipped reverted batch previously and are now
            // discovering more reverted batches.
            tracing::warn!(
                batch_number,
                "non-sequential batch discovered; assuming revert and skipping"
            );
        } else {
            tracing::debug!(batch_number, "discovered committed batch");
            let committed_batch = self.parse_committed_batch(report, log).await?;
            // Wait until discovered batch is executed. Note: this will `await` for the entire time
            // between L1 commit and L1 execute (potentially minutes or even hours).
            //
            // This logic is not totally resistant to reorgs. If `executeBatches` is reverted + the
            // batch itself is reverted then the storage will persist an incorrect batch. The
            // situation should be extremely rare but still possible. Two options here:
            // 1. Trim batches that are no longer executed from the storage on start-up.
            // 2. Track **finalized** executions along with regular (latest) ones. They cannot
            //    be reorged and hence would be safe to depend on here.
            //
            // AFAIU (2) can also help with the similar priority tree issue.
            self.finality
                .subscribe()
                .wait_for(|f| f.last_executed_batch >= batch_number)
                .await
                .map_err(anyhow::Error::from)
                .map_err(L1WatcherError::Other)?;
            let discovered_batch_hash = committed_batch.hash();
            let stored_batch_hash = self.zk_chain.stored_batch_hash(batch_number).await?;
            if stored_batch_hash != discovered_batch_hash {
                // Discovered batch commitment does not match latest L1 state. Likely it got
                // reverted at some point and we will discover another commitment.
                tracing::warn!(
                    ?discovered_batch_hash,
                    ?stored_batch_hash,
                    batch_number,
                    "batch hash mismatch; ignoring"
                );
            }

            self.batch_storage.write(committed_batch);
        }
        Ok(())
    }
}
