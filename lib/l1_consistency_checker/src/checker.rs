use crate::cache::{LocalBatchBlockData, TreeBlockCache};
use async_trait::async_trait;
use std::ops::RangeInclusive;
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
    cache: watch::Sender<TreeBlockCache>,
    latest_verified_batch_tx: watch::Sender<u64>,
    l1_events_rx: PeekableReceiver<L1ConsistencyCheckRequest>,
}

impl<ReadState> L1ConsistencyChecker<ReadState> {
    pub fn new(
        chain_id: u64,
        sl_chain_id: u64,
        last_persisted_block_on_start: u64,
        read_state: ReadState,
        cache: watch::Sender<TreeBlockCache>,
        latest_verified_batch_tx: watch::Sender<u64>,
        l1_events_rx: mpsc::Receiver<L1ConsistencyCheckRequest>,
    ) -> Self {
        Self {
            chain_id,
            sl_chain_id,
            last_persisted_block_on_start,
            read_state,
            cache,
            latest_verified_batch_tx,
            l1_events_rx: PeekableReceiver::new(l1_events_rx),
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
        let data = LocalBatchBlockData {
            output: tree_block.output,
            record: tree_block.record,
            tree_output: tree_block.tree.output,
            multichain_root,
        };

        let mut result = Ok(());
        self.cache
            .send_if_modified(|cache| match cache.insert(block_number, data) {
                Ok(()) => true,
                Err(err) => {
                    result = Err(err);
                    false
                }
            });
        result
    }

    /// Tries to verify the next L1 consistency request without blocking.
    ///
    /// Returns `None` if no request is currently available, `Some(false)` if the
    /// request is waiting for more cached blocks, and `Some(true)` if it was
    /// verified and consumed.
    fn try_verify_pending_request(&mut self) -> anyhow::Result<Option<bool>> {
        let chain_id = self.chain_id;
        let sl_chain_id = self.sl_chain_id;
        let last_persisted_block_on_start = self.last_persisted_block_on_start;
        let cache = self.cache.clone();

        let Some(pending) = self.l1_events_rx.peek() else {
            return Ok(None);
        };
        if !Self::verify_commit_if_available(
            chain_id,
            sl_chain_id,
            last_persisted_block_on_start,
            &cache,
            &pending.commit,
        )? {
            return Ok(Some(false));
        }

        let batch_number = pending.commit.batch_number();
        let range = pending.commit.range.clone();
        let last_block_number = pending.commit.last_block_number();
        self.l1_events_rx
            .pop_buffer()
            .expect("verified L1 consistency check request should remain buffered");
        tracing::info!(
            "verified L1 committed batch #{} against locally replayed blocks {:?}",
            batch_number,
            range
        );
        self.cache
            .send_modify(|cache| cache.remove_lower_or_equal_than(last_block_number));
        self.mark_batch_verified(batch_number);

        Ok(Some(true))
    }

    /// Verifies L1 consistency requests until none are available or the head waits for cache data.
    ///
    /// Returns `true` if a request is still waiting for more cached blocks.
    fn verify_available_requests(&mut self) -> anyhow::Result<bool> {
        loop {
            match self.try_verify_pending_request()? {
                Some(true) => {}
                Some(false) => return Ok(true),
                None => return Ok(false),
            };
        }
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
    fn verify_commit_if_available(
        chain_id: u64,
        sl_chain_id: u64,
        last_persisted_block_on_start: u64,
        cache: &watch::Sender<TreeBlockCache>,
        commit: &L1CommittedBatch,
    ) -> anyhow::Result<bool> {
        // In case we received request for batch that was already persisted, we trust that it was verified previously
        if commit.last_block_number() <= last_persisted_block_on_start {
            return Ok(true);
        }

        let Some(blocks) = cache.borrow().get_range(commit.range.clone())? else {
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
            chain_id,
            commit.batch_number(),
            PubdataMode::from_da_commitment_scheme(commit.batch_info.l2_da_commitment_scheme),
            sl_chain_id,
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
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            let waiting_for_cache = self.verify_available_requests()?;

            tokio::select! {
                tree_block = input.recv() => {
                    let Some(tree_block) = tree_block else {
                        if waiting_for_cache {
                            anyhow::bail!("tree block channel closed with pending L1 consistency check");
                        }
                        return Ok(());
                    };
                    state_reporter.enter_state(GenericComponentState::Active);
                    let block_number = tree_block.record.block_context.block_number;
                    let block_timestamp = tree_block.record.block_context.timestamp;
                    self.insert_tree_block(tree_block)?;
                    state_reporter.record_processed(block_number, Some(block_timestamp), None);
                }
                event = self.l1_events_rx.peek_recv(|request| {
                    (request.commit.batch_number(), request.commit.range.clone())
                }), if !waiting_for_cache => {
                    match event {
                        Some((batch_number, range)) => {
                            state_reporter.enter_state(GenericComponentState::Active);
                            tracing::debug!(
                                "received L1 committed batch {} for consistency checking in range {:?}",
                                batch_number,
                                range,
                            );
                        }
                        None => {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}
