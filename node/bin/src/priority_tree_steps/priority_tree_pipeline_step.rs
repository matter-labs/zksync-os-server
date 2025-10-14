use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_priority_tree::PriorityTreeManager;
use zksync_os_storage_api::{ReadBatch, ReadFinality, ReadReplay};

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
pub struct PriorityTreePipelineStep<BlockStorage, Finality, BatchStorage> {
    priority_tree_manager: PriorityTreeManager<BlockStorage, Finality, BatchStorage>,
}

impl<BlockStorage, Finality, BatchStorage>
    PriorityTreePipelineStep<BlockStorage, Finality, BatchStorage>
where
    BlockStorage: ReadReplay + Clone + Send + Sync + 'static,
    Finality: ReadFinality + Clone + Send + 'static,
    BatchStorage: ReadBatch + Clone + Send + Sync + 'static,
{
    pub async fn new(
        block_storage: BlockStorage,
        db_path: &Path,
        batch_storage: BatchStorage,
        finality: Finality,
    ) -> anyhow::Result<Self> {
        let finality_status = finality.get_finality_status();
        let priority_tree_manager = PriorityTreeManager::new(
            block_storage,
            db_path,
            finality.clone(),
            batch_storage,
            finality_status.last_executed_batch,
        )
        .await?;

        Ok(Self {
            priority_tree_manager,
        })
    }
}

#[async_trait]
impl<BlockStorage, Finality, BatchStorage> PipelineComponent
    for PriorityTreePipelineStep<BlockStorage, Finality, BatchStorage>
where
    BlockStorage: ReadReplay + Clone + Send + Sync + 'static,
    Finality: ReadFinality + Clone + Send + 'static,
    BatchStorage: ReadBatch + Clone + Send + Sync + 'static,
{
    type Input = BatchEnvelope<FriProof>;
    type Output = ExecuteCommand;

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

        // Spawn the three tasks that make up the priority tree subsystem
        let prepare_task = tokio::spawn({
            async move {
                priority_tree_manager_for_prepare
                    .prepare_execute_commands(Some((input, output)), priority_txs_internal_sender)
                    .await
            }
        });

        let keep_caching_task = tokio::spawn({
            async move {
                priority_tree_manager_for_caching
                    .keep_caching(priority_txs_internal_receiver)
                    .await
            }
        });

        // Wait for any task to complete (they should all run indefinitely)
        tokio::select! {
            _ = prepare_task => {
                anyhow::bail!("Priority tree prepare_execute_commands ended unexpectedly")
            }
            _ = keep_caching_task => {
                anyhow::bail!("Priority tree keep_caching ended unexpectedly")
            }
        }
    }
}
