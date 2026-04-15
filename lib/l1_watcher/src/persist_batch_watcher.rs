use crate::traits::ProcessRawEvents;
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{L1WatcherConfig, util};
use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Log, Topic, ValueOrArray};
use alloy::sol_types::SolEvent;
use std::collections::HashMap;
use zksync_os_batch_types::{BatchInfo, DiscoveredCommittedBatch};
use zksync_os_contract_interface::IExecutor::{BlockExecution, ReportCommittedBatchRangeZKsyncOS};
use zksync_os_contract_interface::ZkChain;
use zksync_os_storage_api::{PersistedBatch, WriteBatch};

/// Watches commit and execute events together and persists only irreversibly executed batches.
///
/// This component keeps committed batches in memory until the matching `BlockExecution` event
/// arrives, and only then writes a `PersistedBatch` through `WriteBatch`. That split avoids having
/// to roll back persistent storage for batches that were committed but later reverted on L1.
///
/// Depended on by:
/// - `ExecutedBatchStorage`, which is the concrete persistent store typically passed into this
///   watcher;
/// - `RpcStorage` and RPC namespaces, which read persisted batch data to answer batch- and
///   proof-related requests;
pub struct L1PersistBatchWatcher<BatchStorage> {
    zk_chain: ZkChain<DynProvider>,
    batch_storage: BatchStorage,
    committed_batches: HashMap<u64, DiscoveredCommittedBatch>,
    last_processed_commit_batch: u64,
    last_persisted_batch_on_start: u64,
}

impl<BatchStorage: WriteBatch> L1PersistBatchWatcher<BatchStorage> {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        batch_storage: BatchStorage,
        l1_chain_id: u64,
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
            committed_batches: HashMap::new(),
            last_processed_commit_batch: last_persisted_batch,
            last_persisted_batch_on_start: last_persisted_batch,
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
            Box::new(this),
        )
        .await?;

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

    async fn process_commit(
        &mut self,
        report: ReportCommittedBatchRangeZKsyncOS,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let batch_number = report.batchNumber;
        let latest_processed_batch = self.last_processed_commit_batch;
        let stored_batch = self
            .batch_storage
            .get_batch_by_number(batch_number)
            .map_err(L1WatcherError::Other)?;
        if batch_number <= latest_processed_batch
            && let Some(stored_batch) = stored_batch
        {
            tracing::debug!(
                batch_number,
                "discovered already processed batch, validating"
            );
            let committed_batch = self.parse_committed_batch(report, log).await?;
            if stored_batch.committed_batch != committed_batch {
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
        } else {
            if batch_number > latest_processed_batch + 1 {
                if latest_processed_batch == 0 {
                    // We did not have `ReportCommittedBatchRangeZKsyncOS` event on some of the older
                    // testnet chains (e.g. `stage`, `testnet-alpha`). These batches are considered to
                    // be legacy and are not persisted in batch storage. Users will not be able to
                    // generate L2->L1 log proofs for those batches through RPC.
                    tracing::warn!(
                        batch_number,
                        "first discovered batch #{batch_number} is not batch #1; assuming batches #1-#{} are legacy and skipping them",
                        batch_number - 1
                    );
                } else {
                    // This should only be possible if we skipped reverted batch previously and are now
                    // discovering more reverted batches.
                    tracing::warn!(
                        batch_number,
                        latest_processed_batch,
                        "non-sequential batch discovered; assuming revert and skipping"
                    );
                    return Ok(());
                }
            } else if batch_number <= latest_processed_batch {
                tracing::warn!(
                    "Found already committed batch #{batch_number}, but it is not present in batch storage; \
                    assuming previous operation was reverted and overwriting data"
                );
            }
            tracing::debug!(batch_number, "discovered committed batch");
            let committed_batch = self.parse_committed_batch(report, log).await?;

            self.committed_batches.insert(batch_number, committed_batch);
            self.last_processed_commit_batch = batch_number;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl<BatchStorage: WriteBatch> ProcessRawEvents for L1PersistBatchWatcher<BatchStorage> {
    fn name(&self) -> &'static str {
        "persist_batch"
    }

    fn event_signatures(&self) -> Topic {
        Topic::default()
            .extend(ReportCommittedBatchRangeZKsyncOS::SIGNATURE_HASH)
            .extend(BlockExecution::SIGNATURE_HASH)
    }

    fn contract_addresses(&self) -> ValueOrArray<Address> {
        (*self.zk_chain.address()).into()
    }

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log> {
        logs
    }

    async fn process_raw_event(&mut self, log: Log) -> Result<(), L1WatcherError> {
        let event_signature = log.topics()[0];
        match event_signature {
            s if s == ReportCommittedBatchRangeZKsyncOS::SIGNATURE_HASH => {
                let report = ReportCommittedBatchRangeZKsyncOS::decode_log(&log.inner)?.data;
                self.process_commit(report, log).await?;
            }
            s if s == BlockExecution::SIGNATURE_HASH => {
                // This logic is not totally resistant to reorgs. If `executeBatches` is reverted + the
                // batch itself is reverted then the storage will persist an incorrect batch. The
                // situation should be extremely rare but still possible. Two options here:
                // 1. Trim batches that are no longer executed from the storage on start-up.
                // 2. Track **finalized** executions along with regular (latest) ones. They cannot
                //    be reorged and hence would be safe to depend on here.

                let execute = BlockExecution::decode_log(&log.inner)?.data;
                let batch_number = execute.batchNumber.to::<u64>();
                if batch_number > self.last_persisted_batch_on_start {
                    let batch_hash = execute.batchHash;
                    if let Some(committed_batch) = self.committed_batches.remove(&batch_number) {
                        tracing::debug!(
                            batch_number,
                            ?batch_hash,
                            "discovered executed batch, persisting"
                        );
                        self.batch_storage.write(PersistedBatch {
                            committed_batch,
                            execute_sl_block_number: Some(
                                log.block_number.expect("Missing block number in log"),
                            ),
                        });
                    } else if self.last_processed_commit_batch == self.last_persisted_batch_on_start
                    {
                        // No `ReportCommittedBatchRangeZKsyncOS` event was processed yet, it is very likely that the batch is legacy
                        // i.e. block range was not reported for it. Skip this batch.
                        tracing::info!("assuming batch #{batch_number} is legacy and skipping it");
                    } else {
                        return Err(L1WatcherError::Other(anyhow::anyhow!(
                            "discovered executed batch #{batch_number} was not previously discovered as committed"
                        )));
                    }
                }
            }
            _ => {
                return Err(L1WatcherError::Other(anyhow::anyhow!(
                    "unexpected event topic"
                )));
            }
        }
        Ok(())
    }
}
