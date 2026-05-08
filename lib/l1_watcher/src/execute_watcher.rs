use crate::state_commitment::{StateCommitmentError, StateCommitmentReader};
use crate::watcher::{L1Watcher, L1WatcherError};
use crate::{CommittedBatchProvider, L1WatcherConfig, ProcessL1Event, util};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::Log;
use std::sync::Arc;
use zksync_os_contract_interface::IExecutor::BlockExecution;
use zksync_os_contract_interface::ZkChain;
use zksync_os_storage_api::WriteFinality;

/// Watches settlement-layer execution events and advances the executed finality frontier.
///
/// This component reads `BlockExecution` events, waits until the corresponding committed batch is
/// available in `CommittedBatchProvider`, and then updates `WriteFinality` with the latest
/// executed batch / block numbers.
///
/// Depends on `CommittedBatchProvider` to resolve the executed batch back to its committed block range;
///
/// Depended on by:
/// - `PriorityTreeManager`, which replays and caches priority operations up to the executed
///   frontier;
/// - startup / replay code that reads executed finality to decide where block processing resumes;
/// - RPC-facing storage initialization, which uses executed progress as part of node recovery.
pub struct L1ExecuteWatcher<Finality> {
    inner: ExecuteWatcherState<Finality>,
}

pub struct L1FinalizedExecuteWatcher<Finality> {
    inner: ExecuteWatcherState<Finality>,
}

struct ExecuteWatcherState<Finality> {
    next_batch_number: u64,
    committed_batch_provider: CommittedBatchProvider,
    finality: Finality,
    // Recomputes `state_commitment` from local replay/state to cross-check against L1's value.
    state_commitment_reader: Arc<dyn StateCommitmentReader>,
}

impl<Finality: WriteFinality> L1ExecuteWatcher<Finality> {
    pub async fn create_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        committed_batch_provider: CommittedBatchProvider,
        finality: Finality,
        l1_chain_id: u64,
        state_commitment_reader: Arc<dyn StateCommitmentReader>,
    ) -> anyhow::Result<L1Watcher> {
        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let last_executed_batch = finality.get_finality_status().last_executed_batch;
        tracing::info!(
            current_l1_block,
            last_executed_batch,
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing L1 execute watcher"
        );
        let last_l1_block =
            util::find_l1_execute_block_by_batch_number(zk_chain.clone(), last_executed_batch)
                .await?;
        tracing::info!(last_l1_block, "resolved on L1");

        let this = Self {
            inner: ExecuteWatcherState {
                next_batch_number: last_executed_batch + 1,
                committed_batch_provider,
                finality,
                state_commitment_reader,
            },
        };
        L1Watcher::new(
            config,
            zk_chain.provider().clone(),
            (*zk_chain.address()).into(),
            // We start from last L1 block as it may contain more executed batches apart from the last
            // one.
            last_l1_block,
            None,
            l1_chain_id,
            Box::new(this),
        )
        .await
    }
}

impl<Finality: WriteFinality> L1FinalizedExecuteWatcher<Finality> {
    pub async fn create_finalized_watcher(
        config: L1WatcherConfig,
        zk_chain: ZkChain<DynProvider>,
        committed_batch_provider: CommittedBatchProvider,
        finality: Finality,
        state_commitment_reader: Arc<dyn StateCommitmentReader>,
    ) -> anyhow::Result<L1Watcher> {
        let current_l1_block = zk_chain.provider().get_block_number().await?;
        let last_finalized_executed_batch =
            finality.get_finality_status().last_finalized_executed_batch;
        tracing::info!(
            current_l1_block,
            last_finalized_executed_batch,
            config.max_blocks_to_process,
            ?config.poll_interval,
            zk_chain_address = ?zk_chain.address(),
            "initializing finalized L1 execute watcher"
        );
        let last_l1_block = util::find_l1_execute_block_by_batch_number(
            zk_chain.clone(),
            last_finalized_executed_batch,
        )
        .await?;
        tracing::info!(last_l1_block, "resolved on L1");

        let this = Self {
            inner: ExecuteWatcherState {
                next_batch_number: last_finalized_executed_batch + 1,
                committed_batch_provider,
                finality,
                state_commitment_reader,
            },
        };
        Ok(L1Watcher::new_finalized(
            config,
            zk_chain.provider().clone(),
            (*zk_chain.address()).into(),
            last_l1_block,
            None,
            Box::new(this),
        ))
    }
}

impl<Finality: WriteFinality> ExecuteWatcherState<Finality> {
    async fn process_execution(
        &mut self,
        batch_execute: BlockExecution,
        update_finality: impl FnOnce(&Finality, u64, u64),
        frontier: &'static str,
    ) -> Result<(), L1WatcherError> {
        let batch_number = batch_execute.batchNumber.to::<u64>();
        let batch_hash = batch_execute.batchHash;
        let batch_commitment = batch_execute.commitment;
        if batch_number < self.next_batch_number {
            tracing::debug!(
                batch_number,
                ?batch_hash,
                ?batch_commitment,
                frontier,
                "skipping already processed executed batch",
            );
        } else {
            let discovered_batch = self
                .committed_batch_provider
                .wait_for_batch(batch_number)
                .await;
            let last_executed_block = discovered_batch.last_block_number();

            // Cross-check that the batch L1 just executed matches what the EN replayed locally.
            // We use the `state_commitment` field — fully derivable from RocksDB (no pubdata) —
            // and compare against L1's authoritative value carried in the commit calldata.
            // By execute time, replay is guaranteed to have advanced past `last_executed_block`,
            // so any missing-data error is a real error (no retry).
            verify_state_commitment_at_execute(
                &*self.state_commitment_reader,
                batch_number,
                last_executed_block,
                discovered_batch.batch_info.state_commitment,
                frontier,
            );

            update_finality(&self.finality, batch_number, last_executed_block);
            tracing::info!(
                "discovered executed batch {batch_number}, hash {batch_hash:?}, commitment {batch_commitment:?},\
                last_executed_block {last_executed_block}, frontier {frontier}",
            );
        }
        Ok(())
    }
}

/// Recomputes `state_commitment` from local data and panics if it disagrees with L1's value
/// for this batch. See `commit_watcher::verify_state_commitment` for the rationale; this variant
/// runs without retry because by the time L1 has executed a batch the EN has long since replayed
/// past the corresponding blocks.
fn verify_state_commitment_at_execute(
    reader: &dyn StateCommitmentReader,
    batch_number: u64,
    last_block_number: u64,
    expected: alloy::primitives::B256,
    frontier: &'static str,
) {
    match reader.compute(last_block_number) {
        Ok(actual) => {
            if actual != expected {
                panic!(
                    "state_commitment mismatch at batch {batch_number} (last block \
                     {last_block_number}, frontier {frontier}): local {actual:?}, L1 \
                     {expected:?}. The EN's local replay diverged from the canonical chain; \
                     halting."
                );
            }
        }
        Err(err) => match err {
            StateCommitmentError::MissingTreeInfo(_)
            | StateCommitmentError::MissingBlockHeader(_)
            | StateCommitmentError::MissingReplayRecord(_) => {
                panic!(
                    "state_commitment verification failed at batch {batch_number} (last block \
                     {last_block_number}, frontier {frontier}): {err}. Replay is expected to be \
                     ahead of L1 execution; this indicates a corrupted local store."
                );
            }
            _ => panic!(
                "state_commitment verification failed at batch {batch_number} (last block \
                 {last_block_number}, frontier {frontier}): {err}"
            ),
        },
    }
}

fn update_executed_finality<Finality: WriteFinality>(
    finality: &Finality,
    batch_number: u64,
    last_executed_block: u64,
) {
    finality.update_finality_status(|finality| {
        assert!(
            batch_number > finality.last_executed_batch,
            "non-monotonous executed batch"
        );
        assert!(
            last_executed_block > finality.last_executed_block,
            "non-monotonous executed block"
        );
        finality.last_executed_batch = batch_number;
        finality.last_executed_block = last_executed_block;
    });
}

fn update_finalized_executed_finality<Finality: WriteFinality>(
    finality: &Finality,
    batch_number: u64,
    last_executed_block: u64,
) {
    finality.update_finality_status(|finality| {
        assert!(
            batch_number > finality.last_finalized_executed_batch,
            "non-monotonous finalized executed batch"
        );
        assert!(
            last_executed_block > finality.last_finalized_executed_block,
            "non-monotonous finalized executed block"
        );
        finality.last_finalized_executed_batch = batch_number;
        finality.last_finalized_executed_block = last_executed_block;
    });
}

#[async_trait::async_trait]
impl<Finality: WriteFinality> ProcessL1Event for L1ExecuteWatcher<Finality> {
    const NAME: &'static str = "block_execution";

    type SolEvent = BlockExecution;
    type WatchedEvent = BlockExecution;

    async fn process_event(
        &mut self,
        _provider: &DynProvider,
        batch_execute: BlockExecution,
        _log: Log,
    ) -> Result<(), L1WatcherError> {
        self.inner
            .process_execution(batch_execute, update_executed_finality, "normal")
            .await
    }
}

#[async_trait::async_trait]
impl<Finality: WriteFinality> ProcessL1Event for L1FinalizedExecuteWatcher<Finality> {
    const NAME: &'static str = "finalized_block_execution";

    type SolEvent = BlockExecution;
    type WatchedEvent = BlockExecution;

    async fn process_event(
        &mut self,
        _provider: &DynProvider,
        batch_execute: BlockExecution,
        _log: Log,
    ) -> Result<(), L1WatcherError> {
        self.inner
            .process_execution(
                batch_execute,
                update_finalized_executed_finality,
                "finalized",
            )
            .await
    }
}
