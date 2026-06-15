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
use zksync_os_contract_interface::IExecutor::{
    BlockCommit, BlockExecution, ReportCommittedBatchRangeZKsyncOS,
};
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::settlement_layer_intervals::SettlementLayerIntervals;
use zksync_os_l1_consistency_checker::L1CommittedBatch;
use zksync_os_provider::NodeProvider;
use zksync_os_storage_api::{PersistedBatch, WriteBatch};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct L1BatchCommitment {
    batch_number: u64,
    state_commitment: B256,
    commitment: B256,
}

#[derive(Clone, Debug)]
struct L1BatchRangeReport {
    block_range: RangeInclusive<u64>,
    chain_address: Address,
    commit_tx_hash: TxHash,
    commit_l1_block_number: BlockNumber,
}

#[derive(Clone, Debug, Default)]
struct PendingCommittedBatch {
    commitment: Option<L1BatchCommitment>,
    range_report: Option<L1BatchRangeReport>,
}

#[derive(Clone, Debug)]
enum TrackedBatch {
    Collecting(PendingCommittedBatch),
    AwaitingVerification,
    Ready(DiscoveredCommittedBatch),
}

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
    consistency_checker_tx: Option<mpsc::Sender<L1CommittedBatch>>,
    verified_batches_rx: Option<mpsc::UnboundedReceiver<DiscoveredCommittedBatch>>,
    tracked_batches: HashMap<u64, TrackedBatch>,
    last_processed_commit_batch: u64,
    last_persisted_batch_on_start: u64,
}

impl<BatchStorage: WriteBatch> L1PersistBatchWatcher<BatchStorage> {
    /// Builds an [`SlAwareL1Watcher`](crate::SlAwareL1Watcher) that walks every settlement-layer
    /// interval still relevant to persistence, in order. Per-segment block resolution happens
    /// here; event scanning happens lazily inside the watcher's `run()` loop.
    ///
    /// The migration contract requires `totalBatchesCommitted == totalBatchesExecuted` before a
    /// chain can migrate off an SL (`Migrator.sol`), so each closed interval is self-contained:
    /// every commit on that SL has a matching execute on the same SL, and the in-memory
    /// `tracked_batches` map is empty at interval boundaries.
    #[allow(clippy::too_many_arguments)]
    pub fn create_watcher(
        config: L1WatcherConfig,
        intervals: SettlementLayerIntervals,
        batch_storage: BatchStorage,
        consistency_checker_tx: Option<mpsc::Sender<L1CommittedBatch>>,
        verified_batches_rx: Option<mpsc::UnboundedReceiver<DiscoveredCommittedBatch>>,
    ) -> SegmentResolver<(), Self> {
        assert_eq!(
            consistency_checker_tx.is_some(),
            verified_batches_rx.is_some(),
            "L1 consistency checker sender and verified batch receiver must be configured together"
        );
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
                consistency_checker_tx,
                verified_batches_rx,
                tracked_batches: HashMap::new(),
                last_processed_commit_batch: last_persisted_batch,
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
        commitment: L1BatchCommitment,
        range: &std::ops::RangeInclusive<u64>,
    ) -> Result<(), L1WatcherError> {
        if stored_batch.number() != commitment.batch_number {
            return Err(L1WatcherError::Other(anyhow!(
                "Stored batch number is not matching for batch #{}, stored: {}",
                commitment.batch_number,
                stored_batch.number()
            )));
        }
        if stored_batch.batch_info.state_commitment != commitment.state_commitment {
            return Err(L1WatcherError::Other(anyhow!(
                "State commitment is not matching for batch #{}, stored: {:?}, commit event: {:?}",
                commitment.batch_number,
                stored_batch.batch_info.state_commitment,
                commitment.state_commitment
            )));
        }
        if stored_batch.batch_info.commitment != commitment.commitment {
            return Err(L1WatcherError::Other(anyhow!(
                "Commitment is not matching for batch #{}, stored: {:?}, commit event: {:?}",
                commitment.batch_number,
                stored_batch.batch_info.commitment,
                commitment.commitment
            )));
        }
        if &stored_batch.block_range != range {
            return Err(L1WatcherError::Other(anyhow!(
                "Block range is not matching for batch #{}, stored: {:?}, commit range event: {:?}",
                commitment.batch_number,
                stored_batch.block_range,
                range
            )));
        }
        Ok(())
    }

    async fn process_commitment(
        &mut self,
        provider: &NodeProvider,
        commitment: L1BatchCommitment,
    ) -> Result<(), L1WatcherError> {
        let batch_number = commitment.batch_number;
        {
            let state = self
                .tracked_batches
                .entry(batch_number)
                .or_insert_with(|| TrackedBatch::Collecting(PendingCommittedBatch::default()));
            let TrackedBatch::Collecting(pending) = state else {
                return Err(L1WatcherError::Other(anyhow!(
                    "BlockCommit event for batch #{batch_number} arrived after the batch was already tracked as complete"
                )));
            };
            if let Some(existing) = pending.commitment
                && existing != commitment
            {
                return Err(L1WatcherError::Other(anyhow!(
                    "Conflicting BlockCommit events for batch #{batch_number}"
                )));
            }
            pending.commitment = Some(commitment);
        }
        self.process_complete_commit_if_ready(provider, batch_number)
            .await
    }

    async fn process_commit_range(
        &mut self,
        provider: &NodeProvider,
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
        {
            let state = self
                .tracked_batches
                .entry(batch_number)
                .or_insert_with(|| TrackedBatch::Collecting(PendingCommittedBatch::default()));
            let TrackedBatch::Collecting(pending) = state else {
                return Err(L1WatcherError::Other(anyhow!(
                    "ReportCommittedBatchRangeZKsyncOS event for batch #{batch_number} arrived after the batch was already tracked as complete"
                )));
            };
            if let Some(existing) = &pending.range_report
                && existing.block_range != range_report.block_range
            {
                return Err(L1WatcherError::Other(anyhow!(
                    "Conflicting ReportCommittedBatchRangeZKsyncOS events for batch #{batch_number}"
                )));
            }
            pending.range_report = Some(range_report);
        }
        self.process_complete_commit_if_ready(provider, batch_number)
            .await
    }

    async fn process_complete_commit_if_ready(
        &mut self,
        provider: &NodeProvider,
        batch_number: u64,
    ) -> Result<(), L1WatcherError> {
        let Some(TrackedBatch::Collecting(pending)) = self.tracked_batches.get(&batch_number)
        else {
            return Ok(());
        };
        let (Some(commitment), Some(range_report)) =
            (pending.commitment, pending.range_report.clone())
        else {
            return Ok(());
        };
        self.tracked_batches.remove(&batch_number);

        let latest_processed_batch = self.last_processed_commit_batch;
        let stored_batch = self
            .batch_storage
            .get_batch_by_number(batch_number)
            .map_err(L1WatcherError::Other)?;
        if batch_number <= latest_processed_batch
            && let Some(stored_batch) = stored_batch
        {
            tracing::debug!("discovered already processed batch #{batch_number}, validating");
            Self::validate_stored_batch(
                &stored_batch.committed_batch,
                commitment,
                &range_report.block_range,
            )?;
        } else {
            if batch_number > latest_processed_batch + 1 {
                if latest_processed_batch == 0 {
                    // We did not have `ReportCommittedBatchRangeZKsyncOS` event on some of the older
                    // testnet chains (e.g. `stage`, `testnet-alpha`). These batches are considered to
                    // be legacy and are not persisted in batch storage. Users will not be able to
                    // generate L2->L1 log proofs for those batches through RPC.
                    tracing::warn!(
                        "first discovered batch #{batch_number} is not batch #1; assuming batches #1-#{} are legacy and skipping them",
                        batch_number - 1
                    );
                    self.tracked_batches.retain(|tracked_batch_number, state| {
                        *tracked_batch_number >= batch_number
                            || !matches!(state, TrackedBatch::Collecting(_))
                    });
                } else {
                    // This should only be possible if we skipped reverted batch previously and are now
                    // discovering more reverted batches.
                    tracing::warn!(
                        "non-sequential batch #{batch_number} discovered after latest processed batch #{latest_processed_batch}; assuming revert and skipping"
                    );
                    return Ok(());
                }
            } else if batch_number <= latest_processed_batch {
                tracing::warn!(
                    "Found already committed batch #{batch_number}, but it is not present in batch storage; \
                    assuming previous operation was reverted and overwriting data"
                );
            }
            tracing::debug!("discovered committed batch #{batch_number}");

            // EN-only consistency checker input.
            if let Some(tx) = &self.consistency_checker_tx {
                let l1_commit = L1CommittedBatch {
                    batch_number,
                    state_commitment: commitment.state_commitment,
                    commitment: commitment.commitment,
                    range: range_report.block_range.clone(),
                };

                tx.send(l1_commit).await.map_err(|_| {
                    L1WatcherError::Other(anyhow::anyhow!(
                        "L1 consistency checker event channel closed"
                    ))
                })?;
                self.tracked_batches
                    .insert(batch_number, TrackedBatch::AwaitingVerification);
            } else {
                let committed_batch = self.parse_committed_batch(provider, &range_report).await?;
                Self::validate_stored_batch(
                    &committed_batch,
                    commitment,
                    &range_report.block_range,
                )?;
                self.tracked_batches
                    .insert(batch_number, TrackedBatch::Ready(committed_batch));
            }

            self.last_processed_commit_batch = batch_number;
        }
        Ok(())
    }

    async fn wait_until_batch_verified(
        &mut self,
        batch_number: u64,
    ) -> Result<DiscoveredCommittedBatch, L1WatcherError> {
        let Some(verified_batches_rx) = self.verified_batches_rx.as_mut() else {
            return Err(L1WatcherError::Other(anyhow!(
                "L1 consistency checker is not configured"
            )));
        };
        loop {
            let verified = verified_batches_rx.recv().await.ok_or_else(|| {
                L1WatcherError::Other(anyhow::anyhow!(
                    "L1 consistency checker stopped before verifying batch #{batch_number}"
                ))
            })?;
            let verified_batch_number = verified.number();
            if verified_batch_number == batch_number {
                return Ok(verified);
            }
            self.tracked_batches
                .insert(verified_batch_number, TrackedBatch::Ready(verified));
        }
    }
}

#[async_trait::async_trait]
impl<BatchStorage: WriteBatch> ProcessRawEvents for L1PersistBatchWatcher<BatchStorage> {
    fn name(&self) -> &'static str {
        "persist_batch"
    }

    fn event_signatures(&self) -> Topic {
        Topic::default()
            .extend(BlockCommit::SIGNATURE_HASH)
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
            s if s == BlockCommit::SIGNATURE_HASH => {
                let commit = BlockCommit::decode_log(&log.inner)?.data;
                let commitment = L1BatchCommitment {
                    batch_number: commit.batchNumber.to::<u64>(),
                    state_commitment: commit.batchHash,
                    commitment: commit.commitment,
                };
                self.process_commitment(provider, commitment).await?;
            }
            s if s == ReportCommittedBatchRangeZKsyncOS::SIGNATURE_HASH => {
                let report = ReportCommittedBatchRangeZKsyncOS::decode_log(&log.inner)?.data;
                self.process_commit_range(provider, report, log).await?;
            }
            s if s == BlockExecution::SIGNATURE_HASH => {
                let execute = BlockExecution::decode_log(&log.inner)?.data;
                let batch_number = execute.batchNumber.to::<u64>();
                if batch_number > self.last_persisted_batch_on_start {
                    let batch_hash = execute.batchHash;
                    let committed_batch = match self.tracked_batches.remove(&batch_number) {
                        Some(TrackedBatch::Ready(committed_batch)) => Some(committed_batch),
                        Some(TrackedBatch::AwaitingVerification) => {
                            Some(self.wait_until_batch_verified(batch_number).await?)
                        }
                        Some(state @ TrackedBatch::Collecting(_)) => {
                            self.tracked_batches.insert(batch_number, state);
                            None
                        }
                        None => None,
                    };
                    if let Some(committed_batch) = committed_batch {
                        tracing::debug!(
                            "discovered executed batch #{batch_number}, hash {batch_hash:?}"
                        );

                        if execute.commitment != committed_batch.batch_info.commitment {
                            return Err(L1WatcherError::Other(anyhow!(
                                "Commitment is not matching for batch #{}, commit: {:?}, execute: {:?}",
                                batch_number,
                                committed_batch.batch_info.commitment,
                                execute.commitment
                            )));
                        }

                        if execute.batchHash != committed_batch.batch_info.state_commitment {
                            return Err(L1WatcherError::Other(anyhow!(
                                "State commitment is not matching for batch #{}, commit: {:?}, execute: {:?}",
                                batch_number,
                                committed_batch.batch_info.state_commitment,
                                execute.batchHash
                            )));
                        }

                        tracing::debug!("persisting executed batch #{}", batch_number);
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
                        self.tracked_batches.remove(&batch_number);
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
