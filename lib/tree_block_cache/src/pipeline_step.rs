use crate::LocalBatchDataCache;
use async_trait::async_trait;
use tokio::sync::mpsc;
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{
    ComponentId, HasBlockRangeEnd, PeekableReceiver, PipelineComponent, SendAndRecordExt,
};
use zksync_os_storage_api::{ReadStateHistory, TreeBlock, read_multichain_root};

#[derive(Debug)]
pub struct CachedBlockNotification {
    pub block_number: u64,
    pub block_timestamp: u64,
}

impl HasBlockRangeEnd for CachedBlockNotification {
    fn block_number(&self) -> u64 {
        self.block_number
    }

    fn block_timestamp(&self) -> Option<u64> {
        Some(self.block_timestamp)
    }
}

pub struct TreeBlockCacher<ReadState> {
    cache: LocalBatchDataCache,
    read_state: ReadState,
}

impl<ReadState> TreeBlockCacher<ReadState> {
    pub fn new(cache: LocalBatchDataCache, read_state: ReadState) -> Self {
        Self { cache, read_state }
    }
}

#[async_trait]
impl<ReadState> PipelineComponent for TreeBlockCacher<ReadState>
where
    ReadState: ReadStateHistory + Clone + Send + 'static,
{
    type Input = TreeBlock;
    type Output = CachedBlockNotification;

    const COMPONENT_ID: ComponentId = ComponentId::TreeBlockCacher;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        tracing::info!("starting tree block cacher");
        loop {
            state_reporter.enter_state(GenericComponentState::Idle);
            let Some(tree_block) = input.recv().await else {
                return Ok(());
            };
            state_reporter.enter_state(GenericComponentState::Active);
            let block_number = tree_block.record.block_context.block_number;
            let block_timestamp = tree_block.record.block_context.timestamp;
            let state_view = self.read_state.state_view_at(block_number)?;
            let multichain_root = read_multichain_root(state_view);
            self.cache.insert(tree_block, multichain_root)?;
            output.send_and_record(
                CachedBlockNotification {
                    block_number,
                    block_timestamp,
                },
                &state_reporter,
            )?;
        }
    }
}
