use crate::traits::ProcessRawEvents;
use crate::watcher::L1WatcherError;
use crate::{L1WatcherConfig, SegmentSpec, SlAwareL1Watcher, util};
use alloy::providers::DynProvider;
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use std::collections::HashMap;
use zksync_os_batch_types::DiscoveredCommittedBatch;
use zksync_os_contract_interface::IExecutor::{BlockExecution, ReportCommittedBatchRangeZKsyncOS};
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::settlement_layer_intervals::SettlementLayerIntervals;
use zksync_os_storage_api::{PersistedBatch, WriteBatch};

/// Watches finalized commit and execute events together and persists only irreversibly executed
/// batches.
///
/// This component keeps committed batches in memory until the matching `BlockExecution` event
/// arrives in a finalized settlement-layer block, and only then writes a `PersistedBatch` through
/// `WriteBatch`. That split avoids having to roll back persistent storage for batches that were
/// committed or executed but later reverted on L1.
///
/// Depended on by:
/// - `ExecutedBatchStorage`, which is the concrete persistent store typically passed into this
///   watcher;
/// - `RpcStorage` and RPC namespaces, which read persisted batch data to answer batch- and
///   proof-related requests;
pub struct L1PersistBatchWatcher<BatchStorage> {
    batch_storage: BatchStorage,
    committed_batches: HashMap<u64, DiscoveredCommittedBatch>,
    last_processed_commit_batch: u64,
    last_persisted_batch_on_start: u64,
}

impl<BatchStorage: WriteBatch> L1PersistBatchWatcher<BatchStorage> {
    /// Builds an [`SlAwareL1Watcher`] that walks every settlement-layer interval still relevant
    /// to persistence, in order. Returns cheaply: per-segment block resolution and event
    /// scanning happen lazily inside the watcher's `run()` loop.
    ///
    /// The migration contract requires `totalBatchesCommitted == totalBatchesExecuted` before a
    /// chain can migrate off an SL (`Migrator.sol`), so each closed interval is self-contained:
    /// every commit on that SL has a matching execute on the same SL, and the in-memory
    /// `committed_batches` map is empty at interval boundaries.
    pub fn create_watcher(
        config: L1WatcherConfig,
        intervals: SettlementLayerIntervals,
        batch_storage: BatchStorage,
    ) -> anyhow::Result<SlAwareL1Watcher> {
        let last_persisted_batch = batch_storage.latest_batch();
        tracing::info!(
            last_persisted_batch,
            num_intervals = intervals.intervals().len(),
            config.max_blocks_to_process,
            ?config.poll_interval,
            "initializing L1 persist batch watcher"
        );

        // Build segment specs from the relevant intervals. The first non-skipped segment is
        // adjusted to start at `last_persisted_batch` (so we re-validate it on resume), unless
        // we're at genesis — in which case `0` triggers the batch-0 fast path inside
        // `find_l1_commit_block_by_batch_number` on the watcher side.
        let mut segments = Vec::new();
        for interval in intervals.intervals() {
            // Empty interval: a migration can close without any new batches on the SL.
            if interval
                .last_batch
                .is_some_and(|lb| interval.first_batch > lb)
            {
                continue;
            }
            // Wholly behind `last_persisted_batch`: nothing left to validate or persist here.
            if interval
                .last_batch
                .is_some_and(|lb| last_persisted_batch > lb)
            {
                continue;
            }

            let zk_chain = intervals.resolve_proxy(interval.first_batch)?.clone();
            let first_batch = if segments.is_empty() {
                anyhow::ensure!(
                    interval.first_batch <= last_persisted_batch + 1,
                    "first SL interval ({interval}) must start at or before first non-persisted batch ({})",
                    last_persisted_batch + 1
                );
                last_persisted_batch
            } else {
                // First batch in the interval might not have been committed yet. We will find the
                // canonical start of the segment instead where previous batch got imported during
                // migration.
                interval.first_batch - 1
            };
            segments.push(SegmentSpec {
                zk_chain,
                first_batch,
                last_batch: interval.last_batch,
            });
        }

        anyhow::ensure!(
            !segments.is_empty(),
            "no settlement layer intervals are pending persistence"
        );

        let this = Self {
            batch_storage,
            committed_batches: HashMap::new(),
            last_processed_commit_batch: last_persisted_batch,
            last_persisted_batch_on_start: last_persisted_batch,
        };

        SlAwareL1Watcher::new(config, segments, Box::new(this))
    }

    async fn parse_committed_batch(
        &self,
        provider: &DynProvider,
        report: ReportCommittedBatchRangeZKsyncOS,
        log: Log,
    ) -> Result<DiscoveredCommittedBatch, L1WatcherError> {
        let tx_hash = log.transaction_hash.expect("indexed log without tx hash");
        let zk_chain = ZkChain::new(log.address(), provider.clone());
        let batch_info = util::fetch_committed_batch_data(&zk_chain, tx_hash)
            .await?
            .into_stored();

        Ok(DiscoveredCommittedBatch {
            batch_info,
            block_range: report.firstBlockNumber..=report.lastBlockNumber,
        })
    }

    async fn process_commit(
        &mut self,
        provider: &DynProvider,
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
            let committed_batch = self.parse_committed_batch(provider, report, log).await?;
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
            let committed_batch = self.parse_committed_batch(provider, report, log).await?;

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

    fn filter_events(&self, logs: Vec<Log>) -> Vec<Log> {
        logs
    }

    async fn process_raw_event(
        &mut self,
        provider: &DynProvider,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let event_signature = log.topics()[0];
        match event_signature {
            s if s == ReportCommittedBatchRangeZKsyncOS::SIGNATURE_HASH => {
                let report = ReportCommittedBatchRangeZKsyncOS::decode_log(&log.inner)?.data;
                self.process_commit(provider, report, log).await?;
            }
            s if s == BlockExecution::SIGNATURE_HASH => {
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
