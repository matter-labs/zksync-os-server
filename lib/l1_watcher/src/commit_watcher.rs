use crate::committed_batch_provider::CommittedBatchProvider;
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use zksync_os_batch_types::{BatchInfo, DiscoveredCommittedBatch};
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::ZkChain;
use zksync_os_storage_api::WriteFinality;

pub struct L1CommitWatcher<Finality> {
    zk_chain: ZkChain<DynProvider>,
    next_batch_number: u64,
    committed_batch_provider: CommittedBatchProvider,
    finality: Finality,
}

impl<Finality: WriteFinality> L1CommitWatcher<Finality> {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        committed_batch_provider: CommittedBatchProvider,
        finality: Finality,
        l1_chain_id: u64,
    ) -> anyhow::Result<L1Watcher> {
        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let last_committed_batch = finality.get_finality_status().last_committed_batch;
        tracing::info!(
            current_l1_block,
            last_committed_batch,
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 commit watcher"
        );
        let last_l1_block = util::find_l1_commit_block_by_batch_number(
            zk_chain.clone(),
            last_committed_batch,
            config.max_blocks_to_process,
        )
        .await?;
        tracing::info!(last_l1_block, "resolved on L1");

        let this = Self {
            zk_chain: zk_chain.clone(),
            next_batch_number: last_committed_batch + 1,
            committed_batch_provider,
            finality,
        };
        let l1_watcher = L1Watcher::new(
            zk_chain.provider().clone(),
            // We start from last L1 block as it may contain more committed batches apart from the last
            // one.
            last_l1_block,
            config.max_blocks_to_process,
            config.confirmations,
            l1_chain_id,
            config.poll_interval,
            this.into(),
        )
        .await?;

        Ok(l1_watcher)
    }
}

#[async_trait::async_trait]
impl<Finality: WriteFinality> ProcessL1Event for L1CommitWatcher<Finality> {
    const NAME: &'static str = "block_commit";

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
        if batch_number < self.next_batch_number {
            tracing::debug!(batch_number, "skipping already processed committed batch");
        } else {
            tracing::debug!(batch_number, "discovered committed batch");
            let tx_hash = log.transaction_hash.expect("indexed log without tx hash");
            let committed_batch = util::fetch_commit_calldata(&self.zk_chain, tx_hash).await?;

            // todo: stop using this struct once fully migrated from S3
            let last_executed_batch_info = BatchInfo {
                commit_info: committed_batch.commit_info,
                chain_address: Default::default(),
                upgrade_tx_hash: committed_batch.upgrade_tx_hash,
                blob_sidecar: None,
            };
            let batch_info =
                last_executed_batch_info.into_stored(&committed_batch.protocol_version);
            let committed_batch = DiscoveredCommittedBatch {
                batch_info,
                block_range: report.firstBlockNumber..=report.lastBlockNumber,
            };

            let last_committed_block = committed_batch.last_block_number();
            self.finality.update_finality_status(|finality| {
                assert!(
                    batch_number > finality.last_committed_batch,
                    "non-monotonous committed batch"
                );
                assert!(
                    last_committed_block > finality.last_committed_block,
                    "non-monotonous committed block"
                );
                finality.last_committed_batch = batch_number;
                finality.last_committed_block = last_committed_block;
            });
            self.committed_batch_provider.insert(committed_batch);
        }
        Ok(())
    }
}
