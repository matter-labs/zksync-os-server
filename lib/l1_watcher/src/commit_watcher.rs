use crate::committed_batch_provider::CommittedBatchProvider;
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, ProcessL1Event, util};
use alloy::primitives::Address;
use alloy::providers::DynProvider;
use alloy::rpc::types::Log;
use tokio::sync::watch;
use zksync_os_batch_types::{BatchInfo, DiscoveredCommittedBatch};
use zksync_os_contract_interface::IExecutor::ReportCommittedBatchRangeZKsyncOS;
use zksync_os_contract_interface::ZkChain;
use zksync_os_storage_api::WriteFinality;

pub struct L1CommitWatcher<Finality> {
    zk_chain: ZkChain<DynProvider>,
    next_batch_number: u64,
    // SL tip used for finality initialization. Used to identify historical events during catch-up.
    sl_block_initial_finality_init_at: u64,
    // Last committed batch as of startup. Historical commits above this value are stale.
    startup_last_committed_batch: u64,
    committed_batch_provider: CommittedBatchProvider,
    finality: Finality,
    commit_submitted_rx: Option<watch::Receiver<u64>>,
}

impl<Finality: WriteFinality> L1CommitWatcher<Finality> {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        committed_batch_provider: CommittedBatchProvider,
        finality: Finality,
        sl_block_initial_finality_init_at: u64,
        l1_chain_id: u64,
        commit_submitted_rx: Option<watch::Receiver<u64>>,
    ) -> anyhow::Result<L1Watcher> {
        let last_committed_batch = finality.get_finality_status().last_committed_batch;
        tracing::info!(
            sl_block_initial_finality_init_at,
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
            sl_block_initial_finality_init_at,
            startup_last_committed_batch: last_committed_batch,
            committed_batch_provider,
            finality,
            commit_submitted_rx,
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
        // Startup-only guard: skip historical commits that are above the startup committed frontier.
        // This handles batches that were committed and reverted before the node started.
        if should_skip_historical_commit(
            self.sl_block_initial_finality_init_at,
            self.startup_last_committed_batch,
            batch_number,
            log.block_number,
        ) {
            tracing::warn!(
                batch_number,
                log_block_number = ?log.block_number,
                sl_block_initial_finality_init_at = self.sl_block_initial_finality_init_at,
                startup_last_committed_batch = self.startup_last_committed_batch,
                "skipping historical committed batch above startup frontier; likely reverted before startup",
            );
        } else if batch_number < self.next_batch_number {
            tracing::debug!(batch_number, "skipping already processed committed batch");
        } else {
            // Fast-fail if this batch was committed by a prior crashed session's pending tx.
            if should_restart_for_unexpected_commit(batch_number, self.commit_submitted_rx.as_ref())
            {
                return Err(L1WatcherError::UnexpectedCommit(batch_number));
            }

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

/// Returns true if the commit event is for a batch that this session's pipeline has not yet
/// submitted to L1 — indicating a pending tx from a prior crashed session just landed.
fn should_restart_for_unexpected_commit(
    batch_number: u64,
    commit_submitted_rx: Option<&watch::Receiver<u64>>,
) -> bool {
    commit_submitted_rx.is_some_and(|rx| batch_number > *rx.borrow())
}

/// Returns true if the commit event belongs to startup catch-up range and is above the startup
/// committed frontier.
fn should_skip_historical_commit(
    sl_block_initial_finality_init_at: u64,
    startup_last_committed_batch: u64,
    batch_number: u64,
    log_block_number: Option<u64>,
) -> bool {
    log_block_number.is_some_and(|log_block_number| {
        log_block_number <= sl_block_initial_finality_init_at
            && batch_number > startup_last_committed_batch
    })
}

#[cfg(test)]
mod tests {
    use super::should_restart_for_unexpected_commit;
    use super::should_skip_historical_commit;
    use tokio::sync::watch;

    #[test]
    fn skips_historical_batch_above_startup_frontier() {
        assert!(should_skip_historical_commit(100, 10, 11, Some(99)));
        assert!(should_skip_historical_commit(100, 10, 11, Some(100)));
    }

    #[test]
    fn does_not_skip_batch_after_startup_block() {
        assert!(!should_skip_historical_commit(100, 10, 11, Some(101)));
    }

    #[test]
    fn does_not_skip_batch_within_startup_committed_frontier() {
        assert!(!should_skip_historical_commit(100, 10, 10, Some(50)));
        assert!(!should_skip_historical_commit(100, 10, 9, Some(50)));
    }

    #[test]
    fn does_not_skip_when_log_has_no_block_number() {
        assert!(!should_skip_historical_commit(100, 10, 11, None));
    }

    #[test]
    fn restarts_when_batch_exceeds_submitted() {
        let (_tx, rx) = watch::channel(5u64);
        assert!(should_restart_for_unexpected_commit(6, Some(&rx)));
    }

    #[test]
    fn no_restart_when_batch_equals_submitted() {
        let (_tx, rx) = watch::channel(5u64);
        assert!(!should_restart_for_unexpected_commit(5, Some(&rx)));
    }

    #[test]
    fn no_restart_when_batch_below_submitted() {
        let (_tx, rx) = watch::channel(5u64);
        assert!(!should_restart_for_unexpected_commit(4, Some(&rx)));
    }

    #[test]
    fn no_restart_when_rx_is_none() {
        assert!(!should_restart_for_unexpected_commit(100, None));
    }

    #[test]
    fn no_restart_after_pipeline_updates_submitted() {
        let (tx, rx) = watch::channel(5u64);
        tx.send(6).unwrap();
        assert!(!should_restart_for_unexpected_commit(6, Some(&rx)));
    }
}
