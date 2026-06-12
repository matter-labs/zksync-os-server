use crate::cache::TreeBlockCache;
use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use zksync_os_batch_types::BlockCommitmentData;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{ReadStateHistory, TreeBlock, read_multichain_root};

/// Terminal EN pipeline component that folds replayed blocks into the shared
/// [`TreeBlockCache`].
///
/// This is intentionally the *only* thing that runs on the pipeline task: it does no
/// verification. The CPU-heavy L1 commit verification lives in a separate task
/// ([`L1ConsistencyChecker`](crate::checker::L1ConsistencyChecker)) that evicts from the same
/// cache as batches are confirmed. Keeping verification off this task is what prevents it from
/// starving block intake — when the two shared a single `select!` loop, a burst of L1 commit
/// events could stall intake long enough for the upstream pipeline channel to overflow
/// ("consumer is catastrophically behind").
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
    /// Inserts a block into the shared cache, pre-folded into its commitment ingredients so the
    /// cache holds a few hundred bytes per block (plus pubdata) instead of full block data.
    fn insert_tree_block(&self, tree_block: TreeBlock) -> anyhow::Result<()> {
        let block_number = tree_block.record.block_context.block_number;
        // Blocks already covered by a persisted batch were verified by a previous run; the
        // verifier trusts them without rebuilding, so there is nothing to cache.
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
        // Watch the cache so we can wait for the verifier to evict once we hit the soft bound.
        let mut cache_rx = self.cache.subscribe();
        loop {
            // Backpressure: stop pulling blocks once the cache is full and wait for the verifier
            // to confirm and evict committed batches, freeing space. This bounds memory; it
            // assumes the configured bound comfortably exceeds the gap between local replay and
            // L1 commit availability (and any single batch's block span), otherwise intake stalls
            // and the upstream pipeline channel eventually overflows.
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
