use std::path::Path;
use tokio::sync::mpsc;
use zksync_os_priority_tree::PriorityTreeManager;
use zksync_os_storage_api::{ReadBatch, ReadFinality, ReadReplay};

/// Priority Tree manager for External Nodes.
///
/// Unlike the main node version, this:
/// - Doesn't act as pipeline step - launched as a standalone task instead
/// - Doesn't output execute commands (EN doesn't execute on L1)
/// - Watches finalized batch numbers instead of batch envelopes
pub struct PriorityTreeENStep<BlockStorage, Finality, BatchStorage> {
    priority_tree_manager: PriorityTreeManager<BlockStorage, Finality, BatchStorage>,
}

impl<BlockStorage, Finality, BatchStorage> PriorityTreeENStep<BlockStorage, Finality, BatchStorage>
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
        last_ready_batch: u64,
    ) -> anyhow::Result<Self> {
        let priority_tree_manager = PriorityTreeManager::new(
            block_storage,
            db_path,
            finality.clone(),
            batch_storage,
            last_ready_batch,
        )
        .await?;

        Ok(Self {
            priority_tree_manager,
        })
    }

    /// Run the priority tree tasks for EN (doesn't use pipeline framework as it has no I/O)
    pub async fn run(self) -> anyhow::Result<()> {
        // Internal channel for priority tree manager
        let (priority_txs_internal_sender, priority_txs_internal_receiver) =
            mpsc::channel::<(u64, u64, Option<usize>)>(1000);

        // Clone what we need before moving
        let priority_tree_manager_for_prepare = self.priority_tree_manager.clone();
        let priority_tree_manager_for_caching = self.priority_tree_manager;

        // Task 1: Prepare execute commands (but don't send them)
        let prepare_task = tokio::spawn({
            async move {
                priority_tree_manager_for_prepare
                    .prepare_execute_commands(None, priority_txs_internal_sender)
                    .await
            }
        });

        // Task 2: Keep caching
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
