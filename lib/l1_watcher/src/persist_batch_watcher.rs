use crate::sl_aware_watcher::SegmentResolver;
use crate::traits::ProcessRawEvents;
use crate::watcher::L1WatcherError;
use crate::{L1WatcherConfig, SegmentSpec, util};
use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, BlockNumber, TxHash};
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use anyhow::{Context, anyhow};
use std::collections::HashMap;
use std::ops::RangeInclusive;
use tokio::sync::mpsc;
use zksync_os_batch_types::DiscoveredCommittedBatch;
use zksync_os_contract_interface::IExecutor::{BlockExecution, ReportCommittedBatchRangeZKsyncOS};
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::settlement_layer_intervals::SettlementLayerIntervals;
use zksync_os_l1_consistency_checker::L1ExecutedBatch;
use zksync_os_provider::NodeProvider;
use zksync_os_storage_api::{PersistedBatch, WriteBatch};

#[derive(Clone, Debug)]
struct L1BatchRangeReport {
    block_range: RangeInclusive<u64>,
    chain_address: Address,
    commit_tx_hash: TxHash,
    commit_l1_block_number: BlockNumber,
}

#[derive(Clone, Debug)]
struct VerifiedExecutedBatch {
    committed_batch: DiscoveredCommittedBatch,
    execute_sl_block_number: BlockNumber,
}

/// EN-only channels to the L1 consistency checker: outgoing executed-batch verification requests
/// and incoming verified batches. Passed as a single `Option` so the two ends can't be configured
/// independently (main node skips verification and passes `None`).
pub struct ConsistencyCheckerChannels {
    pub tx: mpsc::Sender<L1ExecutedBatch>,
    pub verified_rx: mpsc::UnboundedReceiver<DiscoveredCommittedBatch>,
}

/// Watches finalized batch-range and execute events together and persists only irreversibly
/// executed batches.
///
/// This component keeps committed batch ranges in memory until the matching `BlockExecution`
/// event arrives in a finalized settlement-layer block, and only then writes a `PersistedBatch`
/// through `WriteBatch`. That split avoids having to roll back persistent storage for batches that
/// were committed or executed but later reverted on L1.
///
/// Depended on by:
/// - `ExecutedBatchStorage`, which is the concrete persistent store typically passed into this
///   watcher;
/// - `RpcStorage` and RPC namespaces, which read persisted batch data to answer batch- and
///   proof-related requests;
pub struct L1PersistBatchWatcher<BatchStorage> {
    batch_storage: BatchStorage,
    consistency_checker: Option<ConsistencyCheckerChannels>,
    range_reports: HashMap<u64, L1BatchRangeReport>,
    /// Executed batches awaiting verification by the consistency checker, mapped to the
    /// settlement-layer block their `BlockExecution` event landed in.
    pending_executions: HashMap<u64, BlockNumber>,
    verified_executions: HashMap<u64, VerifiedExecutedBatch>,
    last_scheduled_batch: u64,
    last_processed_batch: u64,
    last_persisted_batch_on_start: u64,
}

impl<BatchStorage: WriteBatch> L1PersistBatchWatcher<BatchStorage> {
    /// Builds an [`SlAwareL1Watcher`](crate::SlAwareL1Watcher) that walks every settlement-layer
    /// interval still relevant to persistence, in order. Per-segment block resolution happens
    /// here; event scanning happens lazily inside the watcher's `run()` loop.
    ///
    /// The migration contract requires `totalBatchesCommitted == totalBatchesExecuted` before a
    /// chain can migrate off an SL (`Migrator.sol`), so each closed interval is self-contained:
    /// every commit on that SL has a matching execute on the same SL, so pending range reports
    /// should be consumed by the interval's execute events.
    pub fn create_watcher(
        config: L1WatcherConfig,
        intervals: SettlementLayerIntervals,
        batch_storage: BatchStorage,
        consistency_checker: Option<ConsistencyCheckerChannels>,
    ) -> SegmentResolver<(), Self> {
        tracing::info!(
            num_intervals = intervals.intervals().len(),
            config.max_blocks_to_process,
            ?config.poll_interval,
            "initializing L1 persist batch watcher"
        );

        let max_blocks_to_process = config.max_blocks_to_process;

        // Per-segment block resolution (and the starting `last_persisted_batch`) are deferred to
        // the watcher's `run()`; only static dependencies are captured here.
        let resolve_segments = move |()| async move {
            let last_persisted_batch = batch_storage.latest_batch();
            tracing::info!(
                last_persisted_batch,
                "resolving L1 persist batch watcher segments"
            );

            // Build segment specs from the relevant intervals. The first non-skipped segment
            // is adjusted to start at `last_persisted_batch` (so we re-validate it on resume),
            // unless we're at genesis — in which case `0` triggers the batch-0 fast path
            // inside `find_l1_commit_block_by_batch_number`.
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
                // Wholly behind `last_persisted_batch`: nothing left to validate or persist.
                if interval
                    .last_batch
                    .is_some_and(|lb| last_persisted_batch > lb)
                {
                    continue;
                }

                let zk_chain = &interval.proxy;
                let first_batch = if is_first {
                    anyhow::ensure!(
                        interval.first_batch <= last_persisted_batch + 1,
                        "first SL interval ({interval}) must start at or before first non-persisted batch ({})",
                        last_persisted_batch + 1
                    );
                    // Skip the deployment-to-first-commit gap when batch 1 exists.
                    if last_persisted_batch == 0
                        && zk_chain
                            .get_total_batches_committed(BlockId::latest())
                            .await
                            .context("while attempting to read total committed batches")?
                            >= 1
                    {
                        1
                    } else {
                        last_persisted_batch
                    }
                } else {
                    // First batch in the interval might not have been committed yet. We
                    // resolve the canonical start of the segment from the previous batch's
                    // import block.
                    interval.first_batch - 1
                };
                is_first = false;

                let start_block = util::find_l1_commit_block_by_batch_number(
                    zk_chain.clone(),
                    first_batch,
                    max_blocks_to_process,
                )
                .await
                .with_context(|| {
                    format!(
                        "failed to find L1 commit for batch #{first_batch} in interval {interval}"
                    )
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
                    address: (*zk_chain.address()).into(),
                    start_block,
                    end_block,
                });
            }

            anyhow::ensure!(
                !segments.is_empty(),
                "no settlement layer intervals are pending persistence"
            );

            let processor = Self {
                batch_storage,
                consistency_checker,
                range_reports: HashMap::new(),
                pending_executions: HashMap::new(),
                verified_executions: HashMap::new(),
                last_scheduled_batch: last_persisted_batch,
                last_processed_batch: last_persisted_batch,
                last_persisted_batch_on_start: last_persisted_batch,
            };
            Ok((segments, processor))
        };

        SegmentResolver::new(config, resolve_segments)
    }

    async fn parse_committed_batch(
        &self,
        provider: &NodeProvider,
        range_report: &L1BatchRangeReport,
    ) -> Result<DiscoveredCommittedBatch, L1WatcherError> {
        let zk_chain = ZkChain::new(range_report.chain_address, provider.clone());
        let batch_info = util::fetch_committed_batch_data(
            &zk_chain,
            range_report.commit_tx_hash,
            range_report.commit_l1_block_number,
        )
        .await?;

        Ok(DiscoveredCommittedBatch {
            batch_info: batch_info.into_stored(),
            block_range: range_report.block_range.clone(),
        })
    }

    fn validate_stored_batch(
        stored_batch: &DiscoveredCommittedBatch,
        batch_number: u64,
        state_commitment: B256,
        commitment: B256,
        range: &RangeInclusive<u64>,
    ) -> Result<(), L1WatcherError> {
        if stored_batch.number() != batch_number {
            return Err(L1WatcherError::Other(anyhow!(
                "Stored batch number is not matching for batch #{}, stored: {}",
                batch_number,
                stored_batch.number()
            )));
        }
        if stored_batch.batch_info.state_commitment != state_commitment {
            return Err(L1WatcherError::Other(anyhow!(
                "State commitment is not matching for batch #{}, stored: {:?}, execute event: {:?}",
                batch_number,
                stored_batch.batch_info.state_commitment,
                state_commitment
            )));
        }
        if stored_batch.batch_info.commitment != commitment {
            return Err(L1WatcherError::Other(anyhow!(
                "Commitment is not matching for batch #{}, stored: {:?}, execute event: {:?}",
                batch_number,
                stored_batch.batch_info.commitment,
                commitment
            )));
        }
        if &stored_batch.block_range != range {
            return Err(L1WatcherError::Other(anyhow!(
                "Block range is not matching for batch #{}, stored: {:?}, commit range event: {:?}",
                batch_number,
                stored_batch.block_range,
                range
            )));
        }
        Ok(())
    }

    fn process_commit_range(
        &mut self,
        report: ReportCommittedBatchRangeZKsyncOS,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let batch_number = report.batchNumber;
        let range_report = L1BatchRangeReport {
            block_range: report.firstBlockNumber..=report.lastBlockNumber,
            chain_address: log.address(),
            commit_tx_hash: log.transaction_hash.expect("indexed log without tx hash"),
            commit_l1_block_number: log.block_number.expect("indexed log without block number"),
        };

        self.range_reports.insert(batch_number, range_report);
        Ok(())
    }

    fn should_schedule_executed_batch(
        &mut self,
        batch_number: u64,
    ) -> Result<bool, L1WatcherError> {
        let latest_scheduled_batch = self.last_scheduled_batch;
        let already_persisted = self
            .batch_storage
            .get_batch_by_number(batch_number)
            .map_err(L1WatcherError::Other)?
            .is_some();
        if batch_number <= latest_scheduled_batch && already_persisted {
            tracing::debug!("batch #{batch_number} already persisted, skipping");
            return Ok(false);
        }

        if batch_number > latest_scheduled_batch.saturating_add(1) {
            if latest_scheduled_batch == 0 {
                // We did not have `ReportCommittedBatchRangeZKsyncOS` event on some of the older
                // testnet chains (e.g. `stage`, `testnet-alpha`). These batches are considered to
                // be legacy and are not persisted in batch storage. Users will not be able to
                // generate L2->L1 log proofs for those batches through RPC.
                tracing::warn!(
                    "first discovered batch #{batch_number} is not batch #1; assuming batches #1-#{} are legacy and skipping them",
                    batch_number - 1
                );
                self.range_reports
                    .retain(|tracked_batch_number, _| *tracked_batch_number >= batch_number);
            } else {
                // This should only be possible if we skipped reverted batch previously and are now
                // discovering more reverted batches.
                tracing::warn!(
                    "non-sequential executed batch #{batch_number} discovered after latest scheduled batch #{latest_scheduled_batch}; assuming revert and skipping"
                );
                return Ok(false);
            }
        } else if batch_number <= latest_scheduled_batch {
            tracing::warn!(
                "Found already executed batch #{batch_number}, but it is not present in batch storage; \
                assuming previous operation was reverted and overwriting data"
            );
        }

        tracing::debug!("discovered executed batch #{batch_number}");
        Ok(true)
    }

    async fn process_executed_batch(
        &mut self,
        provider: &NodeProvider,
        batch_number: u64,
        state_commitment: B256,
        commitment: B256,
        range_report: &L1BatchRangeReport,
        execute_sl_block_number: BlockNumber,
    ) -> Result<(), L1WatcherError> {
        if let Some(consistency_checker) = &self.consistency_checker {
            let l1_execute = L1ExecutedBatch {
                batch_number,
                state_commitment,
                commitment,
                range: range_report.block_range.clone(),
            };

            consistency_checker.tx.send(l1_execute).await.map_err(|_| {
                L1WatcherError::Other(anyhow::anyhow!(
                    "L1 consistency checker event channel closed"
                ))
            })?;

            self.pending_executions
                .insert(batch_number, execute_sl_block_number);
            self.last_scheduled_batch = batch_number;
            self.drain_verified_batches()?;
            Ok(())
        } else {
            let committed_batch = self.parse_committed_batch(provider, range_report).await?;
            Self::validate_stored_batch(
                &committed_batch,
                batch_number,
                state_commitment,
                commitment,
                &range_report.block_range,
            )?;
            self.last_scheduled_batch = batch_number;
            self.persist_batch(committed_batch, execute_sl_block_number);
            Ok(())
        }
    }

    fn handle_verified_batch(
        &mut self,
        verified: DiscoveredCommittedBatch,
    ) -> Result<(), L1WatcherError> {
        let batch_number = verified.number();
        let Some(execute_sl_block_number) = self.pending_executions.remove(&batch_number) else {
            return Err(L1WatcherError::Other(anyhow!(
                "L1 consistency checker verified unexpected batch #{batch_number}"
            )));
        };

        self.verified_executions.insert(
            batch_number,
            VerifiedExecutedBatch {
                committed_batch: verified,
                execute_sl_block_number,
            },
        );
        self.flush_verified_batches();
        Ok(())
    }

    fn next_batch_to_persist(&self) -> Option<u64> {
        if self.last_processed_batch != 0 {
            return Some(self.last_processed_batch + 1);
        }

        self.pending_executions
            .keys()
            .chain(self.verified_executions.keys())
            .min()
            .copied()
    }

    fn flush_verified_batches(&mut self) {
        while let Some(next_batch) = self.next_batch_to_persist() {
            let Some(verified) = self.verified_executions.remove(&next_batch) else {
                break;
            };
            self.persist_batch(verified.committed_batch, verified.execute_sl_block_number);
        }
    }

    /// Pulls one verified batch from the consistency checker without blocking. Returns `Ok(None)`
    /// when nothing is ready (or the checker isn't configured), and errors only if the checker
    /// stopped while executed batches still await verification.
    fn try_recv_verified(&mut self) -> Result<Option<DiscoveredCommittedBatch>, L1WatcherError> {
        let Some(consistency_checker) = self.consistency_checker.as_mut() else {
            return Ok(None);
        };
        match consistency_checker.verified_rx.try_recv() {
            Ok(verified) => Ok(Some(verified)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) if self.pending_executions.is_empty() => {
                Ok(None)
            }
            Err(mpsc::error::TryRecvError::Disconnected) => Err(L1WatcherError::Other(anyhow!(
                "L1 consistency checker stopped before verifying all executed batches"
            ))),
        }
    }

    fn drain_verified_batches(&mut self) -> Result<(), L1WatcherError> {
        while let Some(verified) = self.try_recv_verified()? {
            self.handle_verified_batch(verified)?;
        }
        Ok(())
    }

    async fn finish_pending_verifications(&mut self) -> Result<(), L1WatcherError> {
        self.drain_verified_batches()?;
        while !self.pending_executions.is_empty() {
            let verified =
                {
                    let Some(consistency_checker) = self.consistency_checker.as_mut() else {
                        return Err(L1WatcherError::Other(anyhow!(
                            "L1 consistency checker is not configured"
                        )));
                    };
                    consistency_checker.verified_rx.recv().await.ok_or_else(|| {
                    L1WatcherError::Other(anyhow::anyhow!(
                        "L1 consistency checker stopped before verifying all executed batches"
                    ))
                })?
                };
            self.handle_verified_batch(verified)?;
        }
        Ok(())
    }

    fn persist_batch(&mut self, committed_batch: DiscoveredCommittedBatch, block_number: u64) {
        let batch_number = committed_batch.number();
        tracing::debug!("persisting executed batch #{}", batch_number);
        self.batch_storage.write(PersistedBatch {
            committed_batch,
            execute_sl_block_number: Some(block_number),
        });
        self.last_processed_batch = batch_number;
    }

    async fn process_execution(
        &mut self,
        provider: &NodeProvider,
        execute: BlockExecution,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let batch_number = execute.batchNumber.to::<u64>();
        if batch_number < self.last_persisted_batch_on_start {
            self.range_reports.remove(&batch_number);
            return Ok(());
        }

        let state_commitment = execute.batchHash;
        let commitment = execute.commitment;
        match self.range_reports.remove(&batch_number) {
            Some(range_report) => {
                if !self.should_schedule_executed_batch(batch_number)? {
                    return Ok(());
                }
                self.process_executed_batch(
                    provider,
                    batch_number,
                    state_commitment,
                    commitment,
                    &range_report,
                    log.block_number.expect("Missing block number in log"),
                )
                .await?;
            }
            None if self.last_scheduled_batch == self.last_persisted_batch_on_start => {
                // No `ReportCommittedBatchRangeZKsyncOS` event was processed yet, it is very likely
                // that the batch is legacy, i.e. block range was not reported for it. Skip this batch.
                tracing::info!("assuming batch #{batch_number} is legacy and skipping it");
                return Ok(());
            }
            None => {
                return Err(L1WatcherError::Other(anyhow!(
                    "discovered executed batch #{batch_number} before its block range was reported"
                )));
            }
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
        provider: &NodeProvider,
        log: Log,
    ) -> Result<(), L1WatcherError> {
        let event_signature = log.topics()[0];
        match event_signature {
            s if s == ReportCommittedBatchRangeZKsyncOS::SIGNATURE_HASH => {
                let report = ReportCommittedBatchRangeZKsyncOS::decode_log(&log.inner)?.data;
                self.process_commit_range(report, log)?;
            }
            s if s == BlockExecution::SIGNATURE_HASH => {
                let execute = BlockExecution::decode_log(&log.inner)?.data;
                self.process_execution(provider, execute, log).await?;
            }
            _ => {
                return Err(L1WatcherError::Other(anyhow::anyhow!(
                    "unexpected event topic"
                )));
            }
        }
        Ok(())
    }

    async fn after_poll(&mut self, _provider: &NodeProvider) -> Result<(), L1WatcherError> {
        self.finish_pending_verifications().await
    }
}
