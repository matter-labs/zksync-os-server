use crate::block_replay_storage::BlockReplayStorage;
use std::path::Path;
use tokio::sync::mpsc;
use zksync_os_priority_tree::PriorityTreeManager;
use zksync_os_storage_api::{ReadBatch, ReadFinality};

/// Priority Tree manager for External Nodes.
///
/// Unlike the main node version, this:
/// - Doesn't act as pipeline step - launched as a standalone task instead
/// - Takes block_replay_storage as input for replayed blocks
/// - Doesn't output execute commands (EN doesn't execute on L1)
/// - Receives batch numbers instead of batch envelopes
pub struct PriorityTreeENStep<BatchStorage, Finality> {
    priority_tree_manager: PriorityTreeManager<BlockReplayStorage>,
    batch_storage: BatchStorage,
    block_replay_storage: BlockReplayStorage,
    finality: Finality,
    last_ready_batch: u64,
}

impl<BatchStorage, Finality> PriorityTreeENStep<BatchStorage, Finality>
where
    BatchStorage: ReadBatch + Clone + Send + Sync + 'static,
    Finality: ReadFinality + Clone + Send + 'static,
{
    pub fn new(
        block_storage: BlockReplayStorage,
        init_block: u64,
        db_path: &Path,
        batch_storage: BatchStorage,
        block_replay_storage: BlockReplayStorage,
        finality: Finality,
        last_ready_batch: u64,
    ) -> anyhow::Result<Self> {
        let priority_tree_manager = PriorityTreeManager::new(block_storage, init_block, db_path)?;

        Ok(Self {
            priority_tree_manager,
            batch_storage,
            block_replay_storage,
            finality,
            last_ready_batch,
        })
    }

    /// Run the priority tree tasks for EN (doesn't use pipeline framework as it has no I/O)
    pub async fn run(self) -> anyhow::Result<()> {
        // Internal channels for priority tree manager
        let (priority_txs_internal_sender, priority_txs_internal_receiver) =
            mpsc::channel::<(u64, u64, Option<usize>)>(1000);

        let (executed_batch_numbers_sender_1, executed_batch_numbers_receiver_1) =
            mpsc::channel::<u64>(1000);

        let (executed_batch_numbers_sender_2, executed_batch_numbers_receiver_2) =
            mpsc::channel::<u64>(1000);

        // Clone what we need before moving
        let priority_tree_manager_for_prepare = self.priority_tree_manager.clone();
        let priority_tree_manager_for_caching = self.priority_tree_manager;

        // Task 1: Send executed batch numbers for prepare_execute_commands
        let finalized_blocks_task_1 = tokio::spawn({
            let batch_storage = self.batch_storage.clone();
            let finality = self.finality.clone();
            let block_replay_storage = self.block_replay_storage.clone();
            let last_ready_batch = self.last_ready_batch;
            async move {
                super::super::util::finalized_block_channel::send_executed_and_replayed_batch_numbers(
                    executed_batch_numbers_sender_1,
                    batch_storage,
                    finality,
                    Some(block_replay_storage),
                    last_ready_batch + 1,
                )
                .await
            }
        });

        // Task 2: Prepare execute commands (but don't send them)
        let prepare_task = tokio::spawn({
            let batch_storage = self.batch_storage.clone();
            async move {
                priority_tree_manager_for_prepare
                    .prepare_execute_commands(
                        batch_storage,
                        None,
                        Some(executed_batch_numbers_receiver_1),
                        priority_txs_internal_sender,
                        None, // No execute command sender for EN
                    )
                    .await
            }
        });

        // Task 3: Send executed batch numbers for keep_caching
        let finalized_blocks_task_2 = tokio::spawn({
            let batch_storage = self.batch_storage;
            let finality = self.finality;
            let block_replay_storage = self.block_replay_storage;
            let last_ready_batch = self.last_ready_batch;
            async move {
                super::super::util::finalized_block_channel::send_executed_and_replayed_batch_numbers(
                    executed_batch_numbers_sender_2,
                    batch_storage,
                    finality,
                    Some(block_replay_storage),
                    last_ready_batch + 1,
                )
                .await
            }
        });

        // Task 4: Keep caching
        let keep_caching_task = tokio::spawn({
            async move {
                priority_tree_manager_for_caching
                    .keep_caching(
                        executed_batch_numbers_receiver_2,
                        priority_txs_internal_receiver,
                    )
                    .await
            }
        });

        // Wait for any task to complete (they should all run indefinitely)
        tokio::select! {
            _ = finalized_blocks_task_1 => {
                anyhow::bail!("Priority tree send_executed_batch_numbers#1 ended unexpectedly")
            }
            _ = prepare_task => {
                anyhow::bail!("Priority tree prepare_execute_commands ended unexpectedly")
            }
            _ = finalized_blocks_task_2 => {
                anyhow::bail!("Priority tree send_executed_batch_numbers#2 ended unexpectedly")
            }
            _ = keep_caching_task => {
                anyhow::bail!("Priority tree keep_caching ended unexpectedly")
            }
        }
    }
}
