use crate::model::ReplayRecord;
use tokio::sync::{mpsc, watch};

pub struct Batcher {
    block_replay_receiver: mpsc::Receiver<ReplayRecord>,
    tree_block_watch: watch::Receiver<u64>,
}

impl Batcher {
    pub fn new(
        block_replay_receiver: mpsc::Receiver<ReplayRecord>,
        tree_block_watch: watch::Receiver<u64>,
    ) -> Self {
        Self {
            block_replay_receiver,
            tree_block_watch,
        }
    }

    pub async fn run_loop(mut self) -> anyhow::Result<()> {
        loop {
            // Wait for a replay record from the channel
            match self.block_replay_receiver.recv().await {
                Some(replay_record) => {
                    let block_number = replay_record.context.block_number;
                    
                    tracing::info!(
                        "Batcher received block {} with {} transactions",
                        block_number,
                        replay_record.transactions.len()
                    );

                    // Wait for the tree to process this block
                    self.wait_for_tree_block(block_number).await?;

                    // Both conditions are met: we have the replay record and the tree has processed the block
                    println!("Block {} is ready for batching", block_number);
                    
                    // TODO: Add actual batching logic here
                }
                None => {
                    // Channel closed, exit the loop
                    tracing::info!("Block replay channel closed, exiting batcher");
                    break;
                }
            }
        }
        Ok(())
    }

    async fn wait_for_tree_block(&mut self, target_block: u64) -> anyhow::Result<()> {
        loop {
            let current_tree_block = *self.tree_block_watch.borrow();
            
            if current_tree_block >= target_block {
                tracing::info!(
                    "Tree has processed block {} (current: {})",
                    target_block,
                    current_tree_block
                );
                return Ok(());
            }

            // Wait for the next update from the tree
            self.tree_block_watch.changed().await?;
        }
    }
}