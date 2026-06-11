use crate::replayer::BatchReplayer;
use anyhow::Context;
use async_trait::async_trait;
use std::ops::RangeInclusive;
use tokio::sync::{mpsc, watch};
use zksync_os_contract_interface::models::{DACommitmentScheme, StoredBatchInfo};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{ReadReplay, ReadStateHistory, TreeBlock};
use zksync_os_types::PubdataMode;

pub struct L1CommittedBatch {
    pub stored_batch_info: StoredBatchInfo,
    pub l2_da_commitment_scheme: DACommitmentScheme,
    pub range: RangeInclusive<u64>,
}

impl L1CommittedBatch {
    pub fn batch_number(&self) -> u64 {
        self.stored_batch_info.batch_number
    }

    pub fn last_block_number(&self) -> u64 {
        *self.range.end()
    }
}

/// Terminal EN pipeline component that checks L1-committed batches against locally replayed
/// blocks.
///
/// It keeps no block data of its own: by the time a block reaches this component its replay
/// record, state diffs, and tree version are all persisted, so a committed batch is verified by
/// rebuilding its commitment from storage (see [`BatchReplayer`]) once the local pipeline has
/// caught up with the batch's block range.
pub struct L1ConsistencyChecker<State, Replays> {
    last_persisted_block_on_start: u64,
    replayer: BatchReplayer<State, Replays>,
    /// Highest block processed by the local pipeline. Storage covers everything up to this
    /// number; shared with the batch verification responder, which has the same "wait until the
    /// data is local" need.
    last_processed_block: watch::Sender<u64>,
    latest_verified_batch_tx: watch::Sender<u64>,
    /// Receives L1-committed batches to verify against locally replayed blocks.
    l1_events_rx: mpsc::Receiver<L1CommittedBatch>,
}

impl<State, Replays> L1ConsistencyChecker<State, Replays> {
    pub fn new(
        last_persisted_block_on_start: u64,
        replayer: BatchReplayer<State, Replays>,
        last_processed_block: watch::Sender<u64>,
        latest_verified_batch_tx: watch::Sender<u64>,
        l1_events_rx: mpsc::Receiver<L1CommittedBatch>,
    ) -> Self {
        Self {
            last_persisted_block_on_start,
            replayer,
            last_processed_block,
            latest_verified_batch_tx,
            l1_events_rx,
        }
    }
}

impl<State: ReadStateHistory + Clone, Replays: ReadReplay + Clone>
    L1ConsistencyChecker<State, Replays>
{
    /// Verifies the commit if the local pipeline has caught up with its block range; `Ok(false)`
    /// means "not yet, retry once more blocks are processed".
    async fn verify_commit_if_ready(&self, commit: &L1CommittedBatch) -> anyhow::Result<bool> {
        // A batch that was already persisted before startup was verified by a previous run.
        if commit.last_block_number() <= self.last_persisted_block_on_start {
            return Ok(true);
        }
        if *self.last_processed_block.borrow() < commit.last_block_number() {
            return Ok(false);
        }

        let replayer = self.replayer.clone();
        let range = commit.range.clone();
        let batch_number = commit.batch_number();
        let pubdata_mode = PubdataMode::from_da_commitment_scheme(commit.l2_da_commitment_scheme);
        // Rebuilding re-executes every block of the batch in the VM; keep that off the async
        // runtime.
        let local_batch_info = tokio::task::spawn_blocking(move || {
            replayer.build_batch_info(range, batch_number, pubdata_mode)
        })
        .await
        .context("batch rebuild task panicked")??;

        let local_stored = local_batch_info.into_stored();
        let l1_stored = &commit.stored_batch_info;
        if &local_stored != l1_stored {
            tracing::error!(
                "L1 committed batch #{} is inconsistent with locally replayed blocks, expected: {:?}, received: {:?}",
                commit.batch_number(),
                local_stored,
                l1_stored,
            );
            anyhow::bail!(
                "L1 committed batch #{} is inconsistent with locally replayed blocks",
                commit.batch_number()
            );
        }

        Ok(true)
    }
}

#[async_trait]
impl<State: ReadStateHistory + Clone, Replays: ReadReplay + Clone> PipelineComponent
    for L1ConsistencyChecker<State, Replays>
{
    type Input = TreeBlock;
    type Output = ();

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::L1ConsistencyChecker;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        tracing::info!("starting L1 consistency checker");
        // At most one commit is held outside the channel at a time; later commits stay queued
        // in `l1_events_rx` until this slot is empty again.
        let mut pending: Option<L1CommittedBatch> = None;
        loop {
            if let Some(commit) = &pending
                && self.verify_commit_if_ready(commit).await?
            {
                tracing::info!(
                    "verified L1 committed batch #{} against locally replayed blocks {:?}",
                    commit.batch_number(),
                    commit.range,
                );
                let batch_number = commit.batch_number();
                self.latest_verified_batch_tx.send_if_modified(|latest| {
                    if batch_number > *latest {
                        *latest = batch_number;
                        true
                    } else {
                        false
                    }
                });
                pending = None;
            }

            state_reporter.enter_state(GenericComponentState::Idle);
            tokio::select! {
                tree_block = input.recv() => {
                    let Some(tree_block) = tree_block else {
                        if pending.is_some() {
                            anyhow::bail!("tree block channel closed with pending L1 consistency check");
                        }
                        return Ok(());
                    };
                    state_reporter.enter_state(GenericComponentState::Active);
                    let block_number = tree_block.record.block_context.block_number;
                    let block_timestamp = tree_block.record.block_context.timestamp;
                    // The block's data is already persisted by the time it reaches this terminal
                    // component; only the progress watermark is needed from it.
                    self.last_processed_block.send_replace(block_number);
                    state_reporter.record_processed(block_number, Some(block_timestamp), None);
                }
                commit = self.l1_events_rx.recv(), if pending.is_none() => {
                    let Some(commit) = commit else {
                        return Ok(());
                    };
                    state_reporter.enter_state(GenericComponentState::Active);
                    tracing::debug!(
                        "received L1 committed batch {} for consistency checking in range {:?}",
                        commit.batch_number(),
                        commit.range,
                    );
                    pending = Some(commit);
                }
            }
        }
    }
}
