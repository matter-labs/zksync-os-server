//use crate::storage_metrics::StorageMetrics;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::StorageWrite;

mod config;
mod metrics;
mod persistent_state;
//mod storage_metrics;

pub use config::StateConfig;
pub use persistent_state::PersistentState;

pub type StateView = PersistentState;

/// Container for the two state components.
/// Thread-safe and cheaply clonable.
/// No atomicity guarantees between the components.
///
/// Hint: don't add `block_number` to this struct - manage finality/block_numbers externally
/// read storage_map.block_number() or persistent_preimages.block_number() if required
#[derive(Debug, Clone)]
pub struct StateHandle {
    persistent_state: PersistentState,
    path: PathBuf,
    checkpoints_to_retain: usize,
}

impl StateHandle {
    const CHECKPOINT_PATH: &str = "checkpoint_";

    pub fn new(config: StateConfig) -> Self {
        if config.erase_storage_on_start {
            if fs::exists(config.rocks_db_path.clone()).unwrap() {
                tracing::info!("Erasing state storage");
                fs::remove_dir_all(config.rocks_db_path.clone()).unwrap();
            } else {
                tracing::info!("State storage is already empty - not erasing");
            }
        }

        let persistent_state = PersistentState::new(config.rocks_db_path.clone());

        tracing::info!(
            block = persistent_state.block_number(),
            "Initializing state storage",
        );

        Self {
            persistent_state,
            path: config.rocks_db_path,
            checkpoints_to_retain: config.blocks_to_retain_in_memory,
        }
    }

    /// Returns (state_block_number, preimages_block_number)
    pub fn latest_block_numbers(&self) -> (u64, u64) {
        let latest_block = self.persistent_state.block_number();
        (latest_block, latest_block)
    }

    pub fn state_view_at_block(&self, block_number: u64) -> anyhow::Result<StateView> {
        let latest_block = self.persistent_state.block_number();
        let oldest_block = latest_block - self.checkpoints_to_retain as u64;

        if block_number > latest_block {
            return Err(anyhow::anyhow!(
                "Block number {block_number} is greater than the current block number {latest_block}"
            ));
        } else if block_number < oldest_block {
            return Err(anyhow::anyhow!(
                "Block number {block_number} is older than the oldest checkpoint {oldest_block}"
            ));
        } else {
            let checkpoint_path = self
                .path
                .join(Self::CHECKPOINT_PATH)
                .join(block_number.to_string());

            if fs::exists(checkpoint_path.clone())? {
                let persistent_state = PersistentState::new(checkpoint_path);
                return Ok(persistent_state);
            } else {
                return Err(anyhow::anyhow!(
                    "Checkpoint for block {block_number} does not exist"
                ));
            }
        }
    }

    pub fn add_block_result<'a, J>(
        &self,
        block_number: u64,
        storage_diffs: Vec<StorageWrite>,
        new_preimages: J,
    ) -> anyhow::Result<()>
    where
        J: IntoIterator<Item = (Bytes32, &'a Vec<u8>)>,
    {
        if block_number != self.persistent_state.block_number() + 1 {
            return Err(anyhow::anyhow!(
                "Block number {block_number} is not the next block after latest block {}",
                self.persistent_state.block_number()
            ));
        }

        // Add the block to the state.
        self.persistent_state
            .write_block(block_number, storage_diffs, new_preimages);

        // Create a checkpoint for the current state. We do a checkpoint here because we always want to provide a
        // stable view of the state for reading, so we can't let the other crates read directly from the main DB.
        let checkpoint_path = self
            .path
            .join(Self::CHECKPOINT_PATH)
            .join(block_number.to_string());
        let checkpointer = self.persistent_state.rocks.checkpoint()?;
        checkpointer.create_checkpoint(checkpoint_path)?;

        // Delete the oldest checkpoint that is no longer needed.
        let oldest_block = block_number - self.checkpoints_to_retain as u64;
        let checkpoint_path = self
            .path
            .join(Self::CHECKPOINT_PATH)
            .join(oldest_block.to_string());
        if fs::exists(checkpoint_path.clone())? {
            fs::remove_dir_all(checkpoint_path)?;
        }

        Ok(())
    }

    pub async fn collect_state_metrics(&self, _period: Duration) {
        // let mut ticker = tokio::time::interval(period);
        // let state_handle = self.clone();
        // loop {
        //     ticker.tick().await;
        //     let m = StorageMetrics::collect_metrics(state_handle.clone());
        //     tracing::debug!("{:?}", m);
        // }
    }

    pub async fn compact_periodically(&self, _period: Duration) {
        // no op
    }
}
