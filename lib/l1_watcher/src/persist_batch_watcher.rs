use crate::sl_aware_watcher::SegmentResolver;
use crate::traits::ProcessRawEvents;
use crate::watcher::L1WatcherError;
use crate::{L1WatcherConfig, SegmentSpec, util};
use alloy::eips::BlockId;
use alloy::rpc::types::{Log, Topic};
use alloy::sol_types::SolEvent;
use anyhow::{Context, anyhow};
use std::collections::HashMap;
use tokio::sync::{mpsc, watch};
use zksync_os_batch_types::{DiscoveredCommittedBatch, ExtendedCommitBatchInfo};
use zksync_os_contract_interface::IExecutor::{BlockExecution, ReportCommittedBatchRangeZKsyncOS};
use zksync_os_contract_interface::ZkChain;
use zksync_os_contract_interface::settlement_layer_intervals::SettlementLayerIntervals;
use zksync_os_l1_consistency_checker::L1CommittedBatch;
use zksync_os_provider::NodeProvider;
use zksync_os_storage_api::{PersistedBatch, WriteBatch};

/// Persists only batches whose commit and execute events were finalized.
pub struct L1PersistBatchWatcher<BatchStorage> {
    batch_storage: BatchStorage,
    consistency_checker_tx: Option<mpsc::Sender<L1CommittedBatch>>,
    latest_verified_batch_rx: Option<watch::Receiver<u64>>,
    committed_batches: HashMap<u64, DiscoveredCommittedBatch>,
    last_processed_commit_batch: u64,
    last_persisted_batch_on_start: u64,
}

impl<BatchStorage: WriteBatch> L1PersistBatchWatcher<BatchStorage> {
    /// Builds an SL-aware watcher over the intervals still relevant to persistence.
    #[allow(clippy::too_many_arguments)]
    pub fn create_watcher(
        config: L1WatcherConfig,
        intervals: SettlementLayerIntervals,
        batch_storage: BatchStorage,
        consistency_checker_tx: Option<mpsc::Sender<L1CommittedBatch>>,
        latest_verified_batch_rx: Option<watch::Receiver<u64>>,
    ) -> SegmentResolver<(), Self> {
        assert_eq!(
            consistency_checker_tx.is_some(),
            latest_verified_batch_rx.is_some(),
            "L1 consistency checker sender and latest verified batch receiver must be configured together"
        );
        tracing::info!(
            num_intervals = intervals.intervals().len(),
            config.max_blocks_to_process,
            ?config.poll_interval,
            "initializing L1 persist batch watcher"
        );

        let max_blocks_to_process = config.max_blocks_to_process;

        // Defer segment resolution until the starting batch is known.
        let resolve_segments = move |()| async move {
            let last_persisted_batch = batch_storage.latest_batch();
            tracing::info!(
                last_persisted_batch,
                "resolving L1 persist batch watcher segments"
            );

            let mut segments = Vec::new();
            let mut is_first = true;
            for interval in intervals.intervals() {
                if interval
                    .last_batch
                    .is_some_and(|lb| interval.first_batch > lb)
                {
                    continue;
                }
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
                latest_verified_batch_rx,
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
                    // Older chains may have unreported legacy batches.
                    tracing::warn!(
                        batch_number,
                        "first discovered batch #{batch_number} is not batch #1; assuming batches #1-#{} are legacy and skipping them",
                        batch_number - 1
                    );
                } else {
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

            // EN-only consistency checker input.
            if let Some(tx) = &self.consistency_checker_tx {
                let l1_commit = L1CommittedBatch {
                    stored_batch_info: committed_batch.batch_info.clone(),
                    l2_da_commitment_scheme: batch_info.l2_da_commitment_scheme,
                    range: committed_batch.block_range.clone(),
                };

                tx.send(l1_commit).await.map_err(|_| {
                    L1WatcherError::Other(anyhow::anyhow!(
                        "L1 consistency checker event channel closed"
                    ))
                })?;
            }

            self.committed_batches.insert(batch_number, committed_batch);
            self.last_processed_commit_batch = batch_number;
        }
        Ok(())
    }

    async fn wait_until_batch_verified(&mut self, batch_number: u64) -> Result<(), L1WatcherError> {
        let Some(latest_verified_batch_rx) = self.latest_verified_batch_rx.as_mut() else {
            return Ok(());
        };

        loop {
            if *latest_verified_batch_rx.borrow_and_update() >= batch_number {
                return Ok(());
            }
            latest_verified_batch_rx.changed().await.map_err(|_| {
                L1WatcherError::Other(anyhow::anyhow!(
                    "L1 consistency checker stopped before verifying batch #{batch_number}"
                ))
            })?;
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
                    if let Some(committed_batch) = self.committed_batches.remove(&batch_number) {
                        tracing::debug!(
                            batch_number,
                            ?batch_hash,
                            "discovered executed batch, waiting for consistency check"
                        );

                        self.wait_until_batch_verified(batch_number).await?;

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
                    } else if self.last_processed_commit_batch == self.last_persisted_batch_on_start
                    {
                        // Likely a legacy batch without a reported block range.
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
