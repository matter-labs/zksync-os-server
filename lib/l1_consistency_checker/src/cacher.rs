use crate::cache::TreeBlockCache;
use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use zksync_os_batch_types::BlockCommitmentData;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{ReadStateHistory, TreeBlock, read_multichain_root};

/// EN pipeline sink that folds replayed blocks into the shared [`TreeBlockCache`].
///
/// Verification runs in [`L1ConsistencyChecker`](crate::checker::L1ConsistencyChecker), off the
/// pipeline task, so block intake is not starved by commitment rebuilding.
pub struct LocalBatchDataCacher<ReadState> {
    last_persisted_block_on_start: u64,
    read_state: ReadState,
    cache: watch::Sender<TreeBlockCache>,
}

impl<ReadState> LocalBatchDataCacher<ReadState> {
    pub fn new(
        last_persisted_block_on_start: u64,
        read_state: ReadState,
        cache: watch::Sender<TreeBlockCache>,
    ) -> Self {
        Self {
            last_persisted_block_on_start,
            read_state,
            cache,
        }
    }
}

impl<ReadState: ReadStateHistory> LocalBatchDataCacher<ReadState> {
    /// Inserts pre-folded commitment data for one replayed block.
    fn insert_tree_block(&self, tree_block: TreeBlock) -> anyhow::Result<()> {
        let block_number = tree_block.record.block_context.block_number;
        // Already persisted batches were verified by a previous run.
        if block_number <= self.last_persisted_block_on_start {
            return Ok(());
        }
        let state_view = self.read_state.state_view_at(block_number)?;
        let multichain_root = read_multichain_root(state_view);
        let data = BlockCommitmentData::new(
            &tree_block.output,
            &tree_block.record.transactions,
            &tree_block.tree.output,
            &tree_block.record.block_context.block_hashes.0,
            multichain_root,
            tree_block.record.protocol_version,
        );

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
}

#[async_trait]
impl<ReadState: ReadStateHistory> PipelineComponent for LocalBatchDataCacher<ReadState> {
    type Input = TreeBlock;
    type Output = ();

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::LocalBatchDataCacher;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        tracing::info!("starting local batch data cacher");
        let mut cache_rx = self.cache.subscribe();
        loop {
            // Bound memory by waiting for verified batches to be evicted.
            while !cache_rx.borrow_and_update().has_capacity() {
                state_reporter.enter_state(GenericComponentState::Idle);
                cache_rx.changed().await?;
            }

            state_reporter.enter_state(GenericComponentState::Idle);
            let Some(tree_block) = input.recv().await else {
                return Ok(());
            };
            state_reporter.enter_state(GenericComponentState::Active);
            let block_number = tree_block.record.block_context.block_number;
            let block_timestamp = tree_block.record.block_context.timestamp;
            self.insert_tree_block(tree_block)?;
            state_reporter.record_processed(block_number, Some(block_timestamp), None);
        }
    }
}
