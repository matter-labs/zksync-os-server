use crate::sl_aware_watcher::SegmentResolver;
use crate::traits::ProcessRawEvents;
use crate::watcher::L1WatcherError;
use crate::{L1WatcherConfig, SegmentSpec, util};
use alloy::eips::BlockId;
use alloy::primitives::B256;
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use anyhow::{Context, anyhow};
use std::collections::HashMap;
use std::ops::RangeInclusive;
use tokio::sync::mpsc;
use zksync_os_batch_types::{DiscoveredCommittedBatch, ExtendedCommitBatchInfo};
use zksync_os_contract_interface::IExecutor::{
    BlockCommit, BlockExecution, ReportCommittedBatchRangeZKsyncOS,
};
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::settlement_layer_intervals::SettlementLayerIntervals;
use zksync_os_l1_consistency_checker::L1CommittedBatch;
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
    /// EN-only. When set, the watcher dispatches committed batches to the L1 consistency checker
    /// (built from `BlockCommit` + `ReportCommittedBatchRange` events, no commit calldata) and
    /// persists the locally-rebuilt batch the checker returns via `verified_batch_rx`. When unset
    /// (main node), the watcher rebuilds the batch from commit calldata instead.
    consistency_checker_tx: Option<mpsc::Sender<L1CommittedBatch>>,
    /// EN-only. Verified, locally-rebuilt batches returned by the consistency checker, keyed into
    /// `committed_batches` as they arrive (out of order) and consumed at execute time.
    verified_batch_rx: Option<mpsc::Receiver<DiscoveredCommittedBatch>>,
    /// EN-only buffers for pairing the two commit events by batch number, since their emission
    /// order within the commit tx is not guaranteed: `(state_commitment, commitment)` from
    /// `BlockCommit`, and the block range from `ReportCommittedBatchRange`.
    pending_commit_hashes: HashMap<u64, (B256, B256)>,
    pending_commit_ranges: HashMap<u64, RangeInclusive<u64>>,
    committed_batches: HashMap<u64, DiscoveredCommittedBatch>,
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
    /// `committed_batches` map is empty at interval boundaries.
    #[allow(clippy::too_many_arguments)]
    pub fn create_watcher(
        config: L1WatcherConfig,
        intervals: SettlementLayerIntervals,
        batch_storage: BatchStorage,
        consistency_checker_tx: Option<mpsc::Sender<L1CommittedBatch>>,
        verified_batch_rx: Option<mpsc::Receiver<DiscoveredCommittedBatch>>,
    ) -> SegmentResolver<(), Self> {
        assert_eq!(
            consistency_checker_tx.is_some(),
            verified_batch_rx.is_some(),
            "L1 consistency checker sender and verified-batch receiver must be configured together"
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
                verified_batch_rx,
                pending_commit_hashes: HashMap::new(),
                pending_commit_ranges: HashMap::new(),
                committed_batches: HashMap::new(),
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
        report: ReportCommittedBatchRangeZKsyncOS,
        log: &Log,
    ) -> Result<(ExtendedCommitBatchInfo, DiscoveredCommittedBatch), L1WatcherError> {
        let tx_hash = log.transaction_hash.expect("indexed log without tx hash");
        let l1_block_number = log.block_number.expect("indexed log without block number");
        let zk_chain = ZkChain::new(log.address(), provider.clone());
        let batch_info =
            util::fetch_committed_batch_data(&zk_chain, tx_hash, l1_block_number).await?;

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

            match &self.consistency_checker_tx {
                // EN: dispatch to the consistency checker, pairing this report's block range with
                // the `BlockCommit` hashes. The rebuilt batch comes back via `verified_batch_rx`
                // and is persisted at execute time — no commit calldata is fetched.
                Some(_) => {
                    self.pending_commit_ranges.insert(
                        batch_number,
                        report.firstBlockNumber..=report.lastBlockNumber,
                    );
                    self.try_dispatch_commit(batch_number).await?;
                }
                // Main node: rebuild the batch from commit calldata and buffer it for persistence.
                None => {
                    let (_, committed_batch) =
                        self.parse_committed_batch(provider, report, &log).await?;
                    self.committed_batches.insert(batch_number, committed_batch);
                }
            }

            self.last_processed_commit_batch = batch_number;
        }
        Ok(())
    }

    /// Records a `BlockCommit` event's hashes and tries to pair them with an already-seen report.
    /// EN-only.
    async fn record_block_commit(&mut self, log: &Log) -> Result<(), L1WatcherError> {
        let event = BlockCommit::decode_log(&log.inner)?.data;
        let batch_number = event.batchNumber.to::<u64>();
        // `batchHash` carries the post-batch state commitment for ZKsync OS batches.
        self.pending_commit_hashes
            .insert(batch_number, (event.batchHash, event.commitment));
        self.try_dispatch_commit(batch_number).await
    }

    /// Dispatches a committed batch to the consistency checker once both of its commit events
    /// (`BlockCommit` hashes and `ReportCommittedBatchRange` block range) have been seen. No-op
    /// until then. EN-only.
    async fn try_dispatch_commit(&mut self, batch_number: u64) -> Result<(), L1WatcherError> {
        let (Some(&(state_commitment, commitment)), Some(range)) = (
            self.pending_commit_hashes.get(&batch_number),
            self.pending_commit_ranges.get(&batch_number),
        ) else {
            return Ok(());
        };
        let range = range.clone();
        self.pending_commit_hashes.remove(&batch_number);
        self.pending_commit_ranges.remove(&batch_number);

        let Some(tx) = &self.consistency_checker_tx else {
            return Ok(());
        };
        tx.send(L1CommittedBatch {
            batch_number,
            state_commitment,
            commitment,
            range,
        })
        .await
        .map_err(|_| {
            L1WatcherError::Other(anyhow::anyhow!(
                "L1 consistency checker event channel closed"
            ))
        })
    }

    /// Waits for the consistency checker to return the verified, locally-rebuilt batch, buffering
    /// any earlier-arriving batches into `committed_batches`. EN-only; the main node never calls
    /// this (it rebuilds from calldata and buffers directly).
    async fn wait_for_verified_batch(
        &mut self,
        batch_number: u64,
    ) -> Result<DiscoveredCommittedBatch, L1WatcherError> {
        if let Some(committed_batch) = self.committed_batches.remove(&batch_number) {
            return Ok(committed_batch);
        }
        loop {
            let verified = self
                .verified_batch_rx
                .as_mut()
                .expect("verified-batch receiver is configured on the external node")
                .recv()
                .await
                .ok_or_else(|| {
                    L1WatcherError::Other(anyhow::anyhow!(
                        "L1 consistency checker stopped before verifying batch #{batch_number}"
                    ))
                })?;
            let verified_number = verified.batch_info.batch_number;
            if verified_number == batch_number {
                return Ok(verified);
            }
            self.committed_batches.insert(verified_number, verified);
        }
    }
}

#[async_trait::async_trait]
impl<BatchStorage: WriteBatch> ProcessRawEvents for L1PersistBatchWatcher<BatchStorage> {
    fn name(&self) -> &'static str {
        "persist_batch"
    }

    fn event_signatures(&self) -> Topic {
        let topic = Topic::default()
            .extend(ReportCommittedBatchRangeZKsyncOS::SIGNATURE_HASH)
            .extend(BlockExecution::SIGNATURE_HASH);
        // On the EN the batch is rebuilt locally and matched against the `BlockCommit` hashes
        // (commitment + state commitment), avoiding the commit calldata fetch.
        if self.consistency_checker_tx.is_some() {
            topic.extend(BlockCommit::SIGNATURE_HASH)
        } else {
            topic
        }
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
            s if s == BlockCommit::SIGNATURE_HASH => {
                self.record_block_commit(&log).await?;
            }
            s if s == BlockExecution::SIGNATURE_HASH => {
                let execute = BlockExecution::decode_log(&log.inner)?.data;
                let batch_number = execute.batchNumber.to::<u64>();
                if batch_number > self.last_persisted_batch_on_start {
                    let batch_hash = execute.batchHash;
                    // Legacy batches (older testnet chains) never produced a
                    // `ReportCommittedBatchRangeZKsyncOS`, so no commit was ever processed.
                    let is_legacy =
                        self.last_processed_commit_batch == self.last_persisted_batch_on_start;
                    // Obtain the batch to persist: the consistency checker's locally-rebuilt batch
                    // on the EN, or the calldata-rebuilt buffer on the main node.
                    let committed_batch = if is_legacy {
                        None
                    } else if self.consistency_checker_tx.is_some() {
                        tracing::debug!(
                            batch_number,
                            ?batch_hash,
                            "discovered executed batch, waiting for consistency check"
                        );
                        Some(self.wait_for_verified_batch(batch_number).await?)
                    } else {
                        self.committed_batches.remove(&batch_number)
                    };

                    if let Some(committed_batch) = committed_batch {
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

                        tracing::debug!(
                            "consistency check completed, persisting executed batch #{}",
                            batch_number
                        );
                        self.batch_storage.write(PersistedBatch {
                            committed_batch,
                            execute_sl_block_number: Some(
                                log.block_number.expect("Missing block number in log"),
                            ),
                        });
                    } else if is_legacy {
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
