pub mod config;
mod metrics;
mod persistent_preimages;
pub mod persistent_storage_map;
pub mod storage_map;
mod storage_view;

use std::sync::atomic::Ordering;
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::{PreimageSource, ReadStorage, StorageWrite};
use zksync_storage::RocksDB;
// Re-export commonly used types
use crate::persistent_preimages::{PersistentPreimages, PreimagesCF};
pub use config::StateConfig;
pub use persistent_storage_map::{PersistentStorageMap, StorageMapCF};
pub use storage_map::{Diff, StorageMap};

const STATE_STORAGE_DB_NAME: &str = "state";
const PREIMAGES_STORAGE_DB_NAME: &str = "preimages";

/// Container for the two state components.
/// Thread-safe and cheaply clonable.
/// No atomicity guarantees between the components.
#[derive(Debug, Clone)]
pub struct StateHandle {
    pub storage_map: StorageMap,
    pub persistent_preimages: PersistentPreimages,
}

impl StateHandle {
    pub fn new(config: StateConfig) -> Self {
        let state_db =
            RocksDB::<StorageMapCF>::new(&config.rocks_db_path.join(STATE_STORAGE_DB_NAME))
                .expect("Failed to open State DB");
        let persistent_storage_map = PersistentStorageMap::new(state_db);

        let storage_map =
            StorageMap::new(persistent_storage_map, config.blocks_to_retain_in_memory);

        let preimages_db =
            RocksDB::<PreimagesCF>::new(&config.rocks_db_path.join(PREIMAGES_STORAGE_DB_NAME))
                .expect("Failed to open Preimages DB");

        let persistent_preimages = PersistentPreimages::new(preimages_db);

        let storage_map_block = storage_map.latest_block.load(Ordering::Relaxed);
        let preimages_block = persistent_preimages.rocksdb_block_number();

        tracing::info!(
            storage_map_block = storage_map_block,
            preimages_block = preimages_block,
            "Initializing state storage",
        );

        Self {
            storage_map,
            persistent_preimages,
        }
    }

    pub fn state_providers_at_block(
        &self,
        block_number: u64,
    ) -> anyhow::Result<(impl ReadStorage, impl PreimageSource)> {
        Ok((
            self.storage_map.view_at(block_number)?,
            self.persistent_preimages.clone(),
        ))
    }
    
    pub fn add_block_result(
        &self,
        block_number: u64,
        storage_diffs: Vec<StorageWrite>,
        new_preimages: Vec<(Bytes32, Vec<u8>)>
    ) -> anyhow::Result<()> {
        self.storage_map.add_diff(block_number, storage_diffs);
        self.persistent_preimages.add(block_number, new_preimages);
        Ok(())
    }
}
