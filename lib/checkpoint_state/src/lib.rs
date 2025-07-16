use crate::persistent_state::StateCF;
use crate::state_view::StorageMapView;
use crate::storage_metrics::StorageMetrics;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::{
    LeafProof, PreimageSource, ReadStorage, ReadStorageTree, StorageWrite,
};
use zksync_storage::RocksDB;

mod config;
mod metrics;
mod persistent_state;
mod state_view;
mod storage_metrics;

pub use config::StateConfig;
pub use persistent_state::PersistentState;

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
                fs::remove_dir_all(config.rocks_db_path).unwrap();
            } else {
                tracing::info!("State storage is already empty - not erasing");
            }
        }

        let state_db =
            RocksDB::<StateCF>::new(&config.rocks_db_path).expect("Failed to open State DB");
        let persistent_state = PersistentState::new(state_db);

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
    ///
    /// Hint: we can return `min` of these two values
    /// for all intents and purposes - only returning both values for easier logging/context
    pub fn latest_block_numbers(&self) -> (u64, u64) {
        let latest_block = self.persistent_state.block_number();
        (latest_block, latest_block)
    }

    pub fn state_view_at_block(&self, block_number: u64) -> anyhow::Result<StateView> {
        Ok(StateView {
            storage_map_view: self.storage_map.view_at(block_number)?,
            preimages: self.persistent_preimages.clone(),
        })
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
        let latest_block = self.persistent_state.block_number();

        if block_number != latest_block + 1 {
            return Err(anyhow::anyhow!(
                "Block number {} is not the next block after latest block {}",
                block_number,
                latest_block
            ));
        }

        // Create a checkpoint for the current state before adding the block.
        let checkpoint_path = self
            .path
            .join(Self::CHECKPOINT_PATH)
            .join(latest_block.to_string());
        let checkpointer = self.persistent_state.rocks.checkpoint()?;
        checkpointer.create_checkpoint(checkpoint_path)?;

        // Add the block to the state.
        self.persistent_state
            .add_block(block_number, storage_diffs, new_preimages);

        // Delete the oldest checkpoint that is no longer needed.
        let oldest_block = latest_block - self.checkpoints_to_retain as u64;
        let checkpoint_path = self
            .path
            .join(Self::CHECKPOINT_PATH)
            .join(oldest_block.to_string());
        if fs::exists(checkpoint_path) {
            fs::remove_dir_all(checkpoint_path)?;
        }

        Ok(())
    }

    pub async fn collect_state_metrics(&self, period: Duration) {
        let mut ticker = tokio::time::interval(period);
        let state_handle = self.clone();
        loop {
            ticker.tick().await;
            let m = StorageMetrics::collect_metrics(state_handle.clone());
            tracing::debug!("{:?}", m);
        }
    }

    pub async fn compact_periodically(&self, _period: Duration) {
        // no op
    }

    //////////////////////
    // Internal methods //
    //////////////////////

    pub fn view_at(&self, block_number: u64) -> anyhow::Result<StorageMapView> {
        let latest_block = self.latest_block.load(Ordering::Relaxed);
        let persistent_block_upper_bound = self
            .persistent_storage_map
            .persistent_block_upper_bound
            .load(Ordering::Relaxed);
        let persistent_block_lower_bound = self
            .persistent_storage_map
            .persistent_block_lower_bound
            .load(Ordering::Relaxed);
        tracing::debug!(
            "Creating StorageMapView for block {} with persistence bounds {} to {} and latest block {}",
            block_number,
            persistent_block_lower_bound,
            persistent_block_upper_bound,
            latest_block
        );

        // we cannot provide keys for block N when it's already compacted
        // because view_at(N) should return view for the BEGINNING of block N
        if block_number <= persistent_block_upper_bound {
            return Err(anyhow::anyhow!(
                "Cannot create StorageView for potentially compacted block {} (potentially compacted until {}, at least until {})",
                block_number,
                persistent_block_upper_bound,
                persistent_block_lower_bound
            ));
        }

        if block_number > latest_block + 1 {
            return Err(anyhow::anyhow!(
                "Cannot create StorageView for block {} - latest known block number is {}",
                block_number,
                latest_block
            ));
        }

        Ok(StorageMapView {
            block: block_number,
            // it's important to use lower_bound here since later blocks are not guaranteed to be in rocksDB yet
            base_block: persistent_block_lower_bound,
            diffs: self.diffs.clone(),
            persistent_storage_map: self.persistent_storage_map.clone(),
        })
    }
}
