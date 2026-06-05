use crate::traits::ProcessRawEvents;
use crate::watcher::L1WatcherError;
use crate::{BlockUpdates, L1WatcherConfig, LogsCache, SegmentSpec, SlAwareL1Watcher, util};
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use anyhow::{Context, anyhow};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot, watch};
use zksync_os_batch_types::{DiscoveredCommittedBatch, ExtendedCommitBatchInfo};
use zksync_os_contract_interface::IExecutor::{BlockExecution, ReportCommittedBatchRangeZKsyncOS};
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::settlement_layer_intervals::{
    IntervalSettlementLayer, SettlementLayerIntervals,
};
use zksync_os_l1_consistency_checker::{
    L1CommittedBatch, L1ConsistencyCheckRequest, L1ConsistencyCheckResult,
};
use zksync_os_provider::NodeProvider;
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
    consistency_checker_tx: Option<mpsc::Sender<L1ConsistencyCheckRequest>>,
    committed_batches: HashMap<u64, PendingCommittedBatch>,
    last_processed_commit_batch: u64,
    last_persisted_batch_on_start: u64,
}

struct PendingCommittedBatch {
    committed_batch: DiscoveredCommittedBatch,
    consistency_check_rx: Option<oneshot::Receiver<L1ConsistencyCheckResult>>,
}

impl<BatchStorage: WriteBatch> L1PersistBatchWatcher<BatchStorage> {
    /// Builds an [`SlAwareL1Watcher`] that walks every settlement-layer interval still relevant
    /// to persistence, in order. Per-segment block resolution happens here; event scanning
    /// happens lazily inside the watcher's `run()` loop.
    ///
    /// The migration contract requires `totalBatchesCommitted == totalBatchesExecuted` before a
    /// chain can migrate off an SL (`Migrator.sol`), so each closed interval is self-contained:
    /// every commit on that SL has a matching execute on the same SL, and the in-memory
    /// `committed_batches` map is empty at interval boundaries.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_watcher(
        config: L1WatcherConfig,
        intervals: SettlementLayerIntervals,
        batch_storage: BatchStorage,
        l1_block_updates: watch::Receiver<BlockUpdates>,
        gateway_block_updates: Option<watch::Receiver<BlockUpdates>>,
        l1_logs_cache: LogsCache,
        gateway_logs_cache: Option<LogsCache>,
        consistency_checker_tx: Option<mpsc::Sender<L1ConsistencyCheckRequest>>,
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
        // `find_l1_commit_block_by_batch_number`.
        let mut segments = Vec::new();
        let mut is_first = true;
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

            let zk_chain = &interval.proxy;
            let (block_updates, logs_cache) = match &interval.settlement_layer {
                IntervalSettlementLayer::L1 => (l1_block_updates.clone(), l1_logs_cache.clone()),
                IntervalSettlementLayer::Gateway(_) => (
                    gateway_block_updates.clone().with_context(|| {
                        format!("Gateway block updates are missing for interval {interval}")
                    })?,
                    gateway_logs_cache.clone().with_context(|| {
                        format!("Gateway logs cache is missing for interval {interval}")
                    })?,
                ),
            };
            let first_batch = if is_first {
                anyhow::ensure!(
                    interval.first_batch <= last_persisted_batch + 1,
                    "first SL interval ({interval}) must start at or before first non-persisted batch ({})",
                    last_persisted_batch + 1
                );
                last_persisted_batch
            } else {
                // First batch in the interval might not have been committed yet. We resolve the
                // canonical start of the segment from the previous batch's import block.
                interval.first_batch - 1
            };
            is_first = false;

            let start_block = util::find_l1_commit_block_by_batch_number(
                zk_chain.clone(),
                first_batch,
                config.max_blocks_to_process,
            )
            .await
            .with_context(|| {
                format!("failed to find L1 commit for batch #{first_batch} in interval {interval}")
            })?;
            let end_block = match interval.last_batch {
                Some(last_batch) => Some(
                    util::find_l1_execute_block_by_batch_number(zk_chain.clone(), last_batch)
                        .await
                        .with_context(|| {
                            format!(
                                "failed to find L1 execute for batch #{last_batch} in interval {interval}"
                            )
                        })?,
                ),
                None => None,
            };

            segments.push(SegmentSpec {
                provider: zk_chain.provider().clone(),
                block_updates,
                logs_cache,
                address: (*zk_chain.address()).into(),
                start_block,
                end_block,
            });
        }

        anyhow::ensure!(
            !segments.is_empty(),
            "no settlement layer intervals are pending persistence"
        );

        let this = Self {
            batch_storage,
            consistency_checker_tx,
            committed_batches: HashMap::new(),
            last_processed_commit_batch: last_persisted_batch,
            last_persisted_batch_on_start: last_persisted_batch,
        };

        SlAwareL1Watcher::new(config, segments, Box::new(this))
    }

    async fn parse_committed_batch(
        &self,
        provider: &NodeProvider,
        report: ReportCommittedBatchRangeZKsyncOS,
        log: &Log,
    ) -> Result<(ExtendedCommitBatchInfo, DiscoveredCommittedBatch), L1WatcherError> {
        let tx_hash = log.transaction_hash.expect("indexed log without tx hash");
        let zk_chain = ZkChain::new(log.address(), provider.clone());
        let batch_info = util::fetch_committed_batch_data(&zk_chain, tx_hash).await?;

        let committed_batch = DiscoveredCommittedBatch {
            batch_info: batch_info.clone().into_stored(),
            block_range: report.firstBlockNumber..=report.lastBlockNumber,
        };

        Ok((batch_info, committed_batch))
    }

    async fn process_commit(
        &mut self,
        provider: &NodeProvider,
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
            let (_, committed_batch) = self.parse_committed_batch(provider, report, &log).await?;
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
            let (batch_info, committed_batch) =
                self.parse_committed_batch(provider, report, &log).await?;
            let consistency_check_rx = if let Some(tx) = &self.consistency_checker_tx {
                let (result_tx, result_rx) = oneshot::channel();
                let l1_commit = L1CommittedBatch {
                    batch_info,
                    range: committed_batch.block_range.clone(),
                };
                tx.send(L1ConsistencyCheckRequest {
                    commit: l1_commit,
                    result_tx,
                })
                .await
                .map_err(|_| {
                    L1WatcherError::Other(anyhow::anyhow!(
                        "L1 consistency checker event channel closed"
                    ))
                })?;
                Some(result_rx)
            } else {
                None
            };

            self.committed_batches.insert(
                batch_number,
                PendingCommittedBatch {
                    committed_batch,
                    consistency_check_rx,
                },
            );
            self.last_processed_commit_batch = batch_number;
        }
        Ok(())
    }
}

async fn wait_for_consistency_check(
    batch_number: u64,
    consistency_check_rx: Option<oneshot::Receiver<L1ConsistencyCheckResult>>,
) -> Result<(), L1WatcherError> {
    let Some(consistency_check_rx) = consistency_check_rx else {
        // if there's no receiver, it means that consistency checker was turned off(so this is a main node) - return positive result
        return Ok(());
    };

    match consistency_check_rx.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(L1WatcherError::Other(anyhow::anyhow!(
            "L1 consistency check failed for batch #{batch_number}: {message}"
        ))),
        Err(_) => Err(L1WatcherError::Other(anyhow::anyhow!(
            "L1 consistency checker stopped before verifying batch #{batch_number}"
        ))),
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
        provider: &NodeProvider,
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
                    if let Some(batch) = self.committed_batches.remove(&batch_number) {
                        tracing::debug!(
                            batch_number,
                            ?batch_hash,
                            "discovered executed batch, waiting for consistency check"
                        );

                        // we don't want to persist a batch that wasn't verified for consistency against L1
                        wait_for_consistency_check(batch_number, batch.consistency_check_rx)
                            .await?;

                        if execute.commitment != batch.committed_batch.batch_info.commitment {
                            return Err(L1WatcherError::Other(anyhow!(
                                "Commitment is not matching for batch #{}, commit: {:?}, execute: {:?}",
                                batch_number,
                                batch.committed_batch.batch_info.commitment,
                                execute.commitment
                            )));
                        }

                        // batchHash from execute event is effectively a state commitment
                        if execute.batchHash != batch.committed_batch.batch_info.state_commitment {
                            return Err(L1WatcherError::Other(anyhow!(
                                "State commitment is not matching for batch #{}, commit: {:?}, execute: {:?}",
                                batch_number,
                                batch.committed_batch.batch_info.state_commitment,
                                execute.batchHash
                            )));
                        }

                        tracing::debug!(
                            "consistency check completed, persisting executed batch #{}",
                            batch_number
                        );
                        self.batch_storage.write(PersistedBatch {
                            committed_batch: batch.committed_batch,
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
