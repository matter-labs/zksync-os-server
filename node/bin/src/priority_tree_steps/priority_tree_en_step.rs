use reth_tasks::Runtime;
use std::path::Path;
use tokio::sync::mpsc;
use zksync_os_l1_watcher::CommittedBatchProvider;
use zksync_os_priority_tree::PriorityTreeManager;
use zksync_os_storage_api::{ReadFinality, ReadReplay};

/// Priority Tree manager for External Nodes.
///
/// Unlike the main node version, this:
/// - Doesn't act as pipeline step - launched as a standalone task instead
/// - Doesn't output execute commands (EN doesn't execute on L1)
/// - Watches finalized batch numbers instead of batch envelopes
pub struct PriorityTreeENStep<BlockStorage, Finality> {
    priority_tree_manager: PriorityTreeManager<BlockStorage, Finality>,
}

impl<BlockStorage, Finality> PriorityTreeENStep<BlockStorage, Finality>
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

    /// Run the priority tree tasks for EN (doesn't use pipeline framework as it has no I/O)
    pub fn spawn(self, runtime: &Runtime) {
        // Internal channel for priority tree manager
        let (priority_txs_internal_sender, priority_txs_internal_receiver) =
            mpsc::channel::<(u64, u64, Option<usize>)>(1000);

        // Clone what we need before moving
        let priority_tree_manager_for_prepare = self.priority_tree_manager.clone();
        let priority_tree_manager_for_caching = self.priority_tree_manager;

        runtime.spawn_critical_with_graceful_shutdown_signal(
            "priority tree caching",
            |shutdown| async move {
                tokio::select! {
                    result = priority_tree_manager_for_caching.keep_caching(priority_txs_internal_receiver) => {
                        result.expect("keep_caching");
                    }
                    result = priority_tree_manager_for_prepare.prepare_execute_commands(None, priority_txs_internal_sender) => {
                        result.expect("prepare_execute_commands");
                    }
                    _guard = shutdown => {
                        // Ensures both futures are dropped before we shutdown gracefully. Otherwise
                        // priority tree manager might keep holding DB.
                    }
                }
            },
        );
    }
}
