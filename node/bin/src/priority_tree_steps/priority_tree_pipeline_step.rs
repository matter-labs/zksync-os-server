use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_l1_sender::commands::L1SenderCommand;
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_l1_watcher::CommittedBatchProvider;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_priority_tree::PriorityTreeManager;
use zksync_os_storage_api::{ReadFinality, ReadReplay};

/// Pipeline step for the Priority Tree manager.
///
/// This component:
/// - Receives proven batches from L1 proof sender
/// - Manages the priority operations tree
/// - Outputs execute commands for L1 executor
///
/// Internally manages:
/// - `prepare_execute_commands` task: processes proven batches and generates execute commands
/// - `keep_caching` task: persists priority tree for executed batches
pub struct PriorityTreePipelineStep<BlockStorage, Finality> {
    priority_tree_manager: PriorityTreeManager<BlockStorage, Finality>,
}

impl<BlockStorage, Finality> PriorityTreePipelineStep<BlockStorage, Finality>
where
    BlockStorage: ReadReplay + Clone,
    Finality: ReadFinality + Clone,
{
    pub async fn new(
        block_storage: BlockStorage,
        db_path: &Path,
        finality: Finality,
        committed_batch_provider: CommittedBatchProvider,
    ) -> anyhow::Result<Self> {
        let priority_tree_manager = PriorityTreeManager::new(
            block_storage,
            db_path,
            finality.clone(),
            committed_batch_provider,
        )
        .await?;

        Ok(Self {
            priority_tree_manager,
        })
    }
}

#[async_trait]
impl<BlockStorage, Finality> PipelineComponent for PriorityTreePipelineStep<BlockStorage, Finality>
where
    BlockStorage: ReadReplay + Clone,
    Finality: ReadFinality + Clone,
{
    type Input = SignedBatchEnvelope<FriProof>;
    type Output = L1SenderCommand<ExecuteCommand>;

    const NAME: &'static str = "priority_tree";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        self,
        input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        // Internal channels for priority tree manager
        let (priority_txs_internal_sender, priority_txs_internal_receiver) =
            mpsc::channel::<(u64, u64, Option<usize>)>(1000);

        // Clone what we need before moving into async blocks
        let priority_tree_manager_for_prepare = self.priority_tree_manager.clone();
        let priority_tree_manager_for_caching = self.priority_tree_manager;

        tokio::select! {
            result = priority_tree_manager_for_caching
                        .keep_caching(priority_txs_internal_receiver) => {
                result.expect("keep_caching");
                Ok(())
            }
            result = priority_tree_manager_for_prepare
                .prepare_execute_commands(Some((input, output)), priority_txs_internal_sender) => {
                result.expect("prepare_execute_commands");
                Ok(())
            }
        }
    }
}
