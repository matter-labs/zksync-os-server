use crate::cache::{LocalBatchDataCacheReader, LocalBatchDataCacheWriter};
use async_trait::async_trait;
use std::{collections::VecDeque, ops::RangeInclusive};
use tokio::sync::{mpsc, watch};
use zksync_os_batch_types::ExtendedCommitBatchInfo;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{ReadStateHistory, TreeBlock, read_multichain_root};
use zksync_os_types::PubdataMode;

pub struct L1CommittedBatch {
    pub batch_info: ExtendedCommitBatchInfo,
    pub range: RangeInclusive<u64>,
}

impl L1CommittedBatch {
    pub fn batch_number(&self) -> u64 {
        self.batch_info.batch_number
    }

    pub fn last_block_number(&self) -> u64 {
        self.range
            .clone()
            .last()
            .expect("last block number of batch should exist in range")
    }
}

/// Request to verify that L1 committed batch data matches locally executed blocks.
pub struct L1ConsistencyCheckRequest {
    pub commit: L1CommittedBatch,
}

/// Terminal EN pipeline component that owns local batch data caching and L1 consistency checks.
pub struct L1ConsistencyChecker<ReadState> {
    chain_id: u64,
    sl_chain_id: u64,
    last_persisted_block_on_start: u64,
    read_state: ReadState,
    cache_writer: LocalBatchDataCacheWriter,
    cache_reader: LocalBatchDataCacheReader,
    latest_verified_batch_tx: watch::Sender<u64>,
    l1_events_rx: mpsc::Receiver<L1ConsistencyCheckRequest>,
    pending_requests: VecDeque<L1ConsistencyCheckRequest>,
}

impl<ReadState> L1ConsistencyChecker<ReadState> {
    pub fn new(
        chain_id: u64,
        sl_chain_id: u64,
        last_persisted_block_on_start: u64,
        read_state: ReadState,
        cache_writer: LocalBatchDataCacheWriter,
        latest_verified_batch_tx: watch::Sender<u64>,
        l1_events_rx: mpsc::Receiver<L1ConsistencyCheckRequest>,
    ) -> Self {
        let cache_reader = cache_writer.subscribe();
        Self {
            chain_id,
            sl_chain_id,
            last_persisted_block_on_start,
            read_state,
            cache_writer,
            cache_reader,
            latest_verified_batch_tx,
            l1_events_rx,
            pending_requests: VecDeque::new(),
        }
    }
}

impl<ReadState: ReadStateHistory> L1ConsistencyChecker<ReadState> {
    /// Inserts a block into the shared cache
    fn insert_tree_block(&self, tree_block: TreeBlock) -> anyhow::Result<()> {
        let block_number = tree_block.record.block_context.block_number;
        if block_number <= self.last_persisted_block_on_start {
            return Ok(());
        }
        let state_view = self.read_state.state_view_at(block_number)?;
        let multichain_root = read_multichain_root(state_view);
        self.cache_writer.insert(tree_block, multichain_root)
    }

    /// Checks for pending requests and verifies earliest ones if possible
    fn verify_pending_commits(&mut self) -> anyhow::Result<()> {
        while let Some(pending) = self.pending_requests.pop_front() {
            match self.verify_commit_if_available(&pending.commit) {
                Ok(true) => {
                    let batch_number = pending.commit.batch_number();
                    tracing::info!(
                        "verified L1 committed batch #{} against locally replayed blocks {:?}",
                        batch_number,
                        pending.commit.range
                    );
                    self.cache_writer
                        .remove_lower_than(pending.commit.last_block_number().saturating_add(1));
                    self.mark_batch_verified(batch_number);
                }
                Ok(false) => {
                    // blocks required for consistency check are not available from cache yet, waiting for the next iteration
                    self.pending_requests.push_front(pending);
                    break;
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    fn mark_batch_verified(&self, batch_number: u64) {
        self.latest_verified_batch_tx.send_if_modified(|latest| {
            if batch_number > *latest {
                *latest = batch_number;
                true
            } else {
                false
            }
        });
    }

    /// Verifies whether data received from verification request is consistent with locally replayed data
    fn verify_commit_if_available(&self, commit: &L1CommittedBatch) -> anyhow::Result<bool> {
        // In case we received request for batch that was already persisted, we trust that it was verified previously
        if commit.last_block_number() <= self.last_persisted_block_on_start {
            return Ok(true);
        }

        let Some(blocks) = self.cache_reader.get_range(commit.range.clone())? else {
            // blocks required for consistency check are not available from cache yet
            return Ok(false);
        };

        let first_block = blocks
            .first()
            .expect("L1 committed batch block range cannot be empty");
        let last_block = blocks
            .last()
            .expect("L1 committed batch block range cannot be empty");

        let (local_batch_info, _) = ExtendedCommitBatchInfo::build(
            blocks
                .iter()
                .map(|block| {
                    (
                        &block.output,
                        block.record.transactions.as_slice(),
                        &block.tree_output,
                    )
                })
                .collect(),
            self.chain_id,
            commit.batch_number(),
            PubdataMode::from_da_commitment_scheme(commit.batch_info.l2_da_commitment_scheme),
            self.sl_chain_id,
            last_block.multichain_root,
            &first_block.record.protocol_version,
            &last_block.record.block_context.block_hashes.0,
        );

        let local_stored = local_batch_info.into_stored();
        let l1_stored = commit.batch_info.clone().into_stored();
        if local_stored != l1_stored {
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
impl<ReadState: ReadStateHistory> PipelineComponent for L1ConsistencyChecker<ReadState> {
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
        let mut l1_events_closed = false;
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            tokio::select! {
                tree_block = input.recv() => {
                    let Some(tree_block) = tree_block else {
                        if !self.pending_requests.is_empty() {
                            anyhow::bail!(
                                "tree block channel closed with {} pending L1 consistency checks",
                                self.pending_requests.len()
                            );
                        }
                        return Ok(());
                    };
                    state_reporter.enter_state(GenericComponentState::Active);
                    let block_number = tree_block.record.block_context.block_number;
                    let block_timestamp = tree_block.record.block_context.timestamp;
                    self.insert_tree_block(tree_block)?;

                    // if there is a pending request for verification, it might be waiting for the received block
                    self.verify_pending_commits()?;
                    state_reporter.record_processed(block_number, Some(block_timestamp), None);
                }
                event = self.l1_events_rx.recv(), if !l1_events_closed => {
                    match event {
                        Some(request) => {
                            state_reporter.enter_state(GenericComponentState::Active);
                            tracing::debug!(
                                "received L1 committed batch {} for consistency checking in range {:?}",
                                request.commit.batch_number(),
                                request.commit.range,
                            );
                            self.pending_requests.push_back(request);
                            self.verify_pending_commits()?;
                        }
                        None => {
                            l1_events_closed = true;
                        }
                    }
                }
            }
        }
    }
}
