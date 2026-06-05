use crate::cache::LocalBatchDataCache;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::ops::RangeInclusive;
use tokio::sync::mpsc;
use zksync_os_batch_types::ExtendedCommitBatchInfo;
use zksync_os_contract_interface::models::DACommitmentScheme;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{ReadStateHistory, TreeBlock, read_multichain_root};
use zksync_os_types::PubdataMode;

/// L1 data discovered by the persistence watcher and checked against locally replayed blocks.
#[derive(Clone, Debug)]
pub struct L1CommittedBatch {
    pub batch_info: ExtendedCommitBatchInfo,
    pub block_range: RangeInclusive<u64>,
}

impl L1CommittedBatch {
    pub fn batch_number(&self) -> u64 {
        self.batch_info.batch_number
    }

    fn last_block_number(&self) -> u64 {
        *self.block_range.end()
    }
}

#[derive(Clone, Debug)]
pub enum L1ConsistencyCheckEvent {
    BatchCommitted(L1CommittedBatch),
}

/// Terminal EN pipeline component that owns local batch data caching and L1 consistency checks.
pub struct L1ConsistencyChecker<ReadState> {
    chain_id: u64,
    sl_chain_id: u64,
    first_unpersisted_block: u64,
    read_state: ReadState,
    cache: LocalBatchDataCache,
    l1_events_rx: mpsc::Receiver<L1ConsistencyCheckEvent>,
    pending_commits: VecDeque<L1CommittedBatch>,
}

impl<ReadState> L1ConsistencyChecker<ReadState> {
    pub fn new(
        chain_id: u64,
        sl_chain_id: u64,
        first_unpersisted_block: u64,
        read_state: ReadState,
        cache: LocalBatchDataCache,
        l1_events_rx: mpsc::Receiver<L1ConsistencyCheckEvent>,
    ) -> Self {
        Self {
            chain_id,
            sl_chain_id,
            first_unpersisted_block,
            read_state,
            cache,
            l1_events_rx,
            pending_commits: VecDeque::new(),
        }
    }
}

impl<ReadState: ReadStateHistory> L1ConsistencyChecker<ReadState> {
    fn insert_tree_block(&self, tree_block: TreeBlock) -> anyhow::Result<()> {
        let block_number = tree_block.record.block_context.block_number;
        if block_number < self.first_unpersisted_block {
            return Ok(());
        }
        let state_view = self.read_state.state_view_at(block_number)?;
        let multichain_root = read_multichain_root(state_view);
        self.cache.insert(tree_block, multichain_root)
    }

    fn verify_pending_commits(&mut self) -> anyhow::Result<()> {
        let mut still_pending = VecDeque::new();
        while let Some(commit) = self.pending_commits.pop_front() {
            if self.verify_commit_if_available(&commit)? {
                tracing::info!(
                    batch_number = commit.batch_number(),
                    block_from = *commit.block_range.start(),
                    block_to = *commit.block_range.end(),
                    "verified L1 committed batch against locally replayed blocks"
                );
                self.cache
                    .remove_lower_than(commit.last_block_number().saturating_add(1));
            } else {
                still_pending.push_back(commit);
            }
        }
        self.pending_commits = still_pending;
        Ok(())
    }

    fn verify_commit_if_available(&self, commit: &L1CommittedBatch) -> anyhow::Result<bool> {
        if commit.last_block_number() < self.first_unpersisted_block {
            return Ok(true);
        }
        let Some(blocks) = self.cache.get_range(commit.block_range.clone())? else {
            return Ok(false);
        };
        let first_block = blocks
            .first()
            .expect("L1 committed batch block range cannot be empty");
        let last_block = blocks
            .last()
            .expect("L1 committed batch block range cannot be empty");

        let pubdata_mode = pubdata_mode_for_l1_commit(&commit.batch_info)?;
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
            pubdata_mode,
            self.sl_chain_id,
            last_block.multichain_root,
            &first_block.record.protocol_version,
            &last_block.record.block_context.block_hashes.0,
        );

        let local_stored = local_batch_info.into_stored();
        let l1_stored = commit.batch_info.clone().into_stored();
        if local_stored != l1_stored {
            tracing::error!(
                batch_number = commit.batch_number(),
                ?local_stored,
                ?l1_stored,
                "L1 committed batch is inconsistent with locally replayed blocks"
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
                        if !self.pending_commits.is_empty() {
                            anyhow::bail!(
                                "tree block channel closed with {} pending L1 consistency checks",
                                self.pending_commits.len()
                            );
                        }
                        return Ok(());
                    };
                    state_reporter.enter_state(GenericComponentState::Active);
                    let block_number = tree_block.record.block_context.block_number;
                    let block_timestamp = tree_block.record.block_context.timestamp;
                    self.insert_tree_block(tree_block)?;
                    self.verify_pending_commits()?;
                    state_reporter.record_processed(block_number, Some(block_timestamp), None);
                }
                event = self.l1_events_rx.recv(), if !l1_events_closed => {
                    match event {
                        Some(L1ConsistencyCheckEvent::BatchCommitted(commit)) => {
                            state_reporter.enter_state(GenericComponentState::Active);
                            tracing::debug!(
                                batch_number = commit.batch_number(),
                                block_from = *commit.block_range.start(),
                                block_to = *commit.block_range.end(),
                                "received L1 committed batch for consistency checking"
                            );
                            self.pending_commits.push_back(commit);
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

fn pubdata_mode_for_l1_commit(commit: &ExtendedCommitBatchInfo) -> anyhow::Result<PubdataMode> {
    match commit.l2_da_commitment_scheme {
        DACommitmentScheme::BlobsZKsyncOS => Ok(PubdataMode::Blobs),
        DACommitmentScheme::BlobsAndPubdataKeccak256 => Ok(PubdataMode::Calldata),
        DACommitmentScheme::EmptyNoDA => Ok(PubdataMode::Validium),
        DACommitmentScheme::None | DACommitmentScheme::PubdataKeccak256 => {
            anyhow::bail!(
                "unsupported DA commitment scheme for L1 consistency check: {:?}",
                commit.l2_da_commitment_scheme
            )
        }
    }
}
