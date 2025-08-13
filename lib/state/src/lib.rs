pub mod config;
mod metrics;
mod persistent_preimages;
pub mod persistent_storage_map;
pub mod storage_map;
mod storage_map_view;
mod storage_metrics;

use ruint::aliases::B160;
use std::fs;
use std::sync::atomic::Ordering;
use std::time::Duration;
use zk_ee::common_structs::derive_flat_storage_key;
use zk_ee::utils::Bytes32;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties, address_into_special_storage_key,
};
use zk_os_forward_system::run::{
    LeafProof, PreimageSource, ReadStorage, ReadStorageTree, StorageWrite,
};
use zksync_os_rocksdb::RocksDB;
// Re-export commonly used types
use crate::persistent_preimages::{PersistentPreimages, PreimagesCF};
use crate::storage_map_view::StorageMapView;
use crate::storage_metrics::StorageMetrics;
pub use config::StateConfig;
pub use persistent_storage_map::{PersistentStorageMap, StorageMapCF};
pub use storage_map::{Diff, StorageMap};

const STATE_STORAGE_DB_NAME: &str = "state";
const PREIMAGES_STORAGE_DB_NAME: &str = "preimages";

/// Container for the two state components.
/// Thread-safe and cheaply clonable.
/// No atomicity guarantees between the components.
///
/// Hint: don't add `block_number` to this struct - manage finality/block_numbers externally
/// read storage_map.block_number() or persistent_preimages.block_number() if required
#[derive(Debug, Clone)]
pub struct StateHandle {
    storage_map: StorageMap,
    persistent_preimages: PersistentPreimages,
}

impl StateHandle {
    pub fn new(config: StateConfig) -> Self {
        if config.erase_storage_on_start {
            let path = config.rocks_db_path.join(STATE_STORAGE_DB_NAME);
            if fs::exists(path.clone()).unwrap() {
                tracing::info!("Erasing state storage");
                fs::remove_dir_all(path).unwrap();
            } else {
                tracing::info!("State storage is already empty - not erasing");
            }
        }
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

    pub fn compacted_block_number(&self) -> u64 {
        self.storage_map
            .persistent_storage_map
            .rocksdb_block_number()
    }

    pub fn state_view_at_block(&self, block_number: u64) -> anyhow::Result<StateView> {
        Ok(StateView {
            storage_map_view: self.storage_map.view_at(block_number)?,
            preimages: self.persistent_preimages.clone(),
        })
    }

    /// Adds a block result to the state components
    /// No atomicity guarantees
    /// PreimageType is currently ignored
    pub fn add_block_result<'a, J>(
        &self,
        block_number: u64,
        storage_diffs: Vec<StorageWrite>,
        new_preimages: J,
    ) -> anyhow::Result<()>
    where
        J: IntoIterator<Item = (Bytes32, &'a Vec<u8>)>,
    {
        self.storage_map.add_diff(block_number, storage_diffs);
        self.persistent_preimages.add(block_number, new_preimages);
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

    pub async fn compact_periodically(&self, period: Duration) {
        let mut ticker = tokio::time::interval(period);
        let map = self.storage_map.clone();
        // can take more than `period` to comact - use proper scheduler
        loop {
            ticker.tick().await;
            map.compact();
        }
    }
}

/// Thin wrapper around `StorageMapView` and `PersistentPreimages`
/// Delegates providing states to the respective components
#[derive(Debug, Clone)]
pub struct StateView {
    storage_map_view: StorageMapView,
    preimages: PersistentPreimages,
}

impl PreimageSource for StateView {
    fn get_preimage(&mut self, hash: Bytes32) -> Option<Vec<u8>> {
        self.preimages.get_preimage(hash)
    }
}
impl ReadStorage for StateView {
    fn read(&mut self, key: Bytes32) -> Option<Bytes32> {
        self.storage_map_view.read(key)
    }
}

// temporarily implement ReadStorageTree as interface requires that
impl ReadStorageTree for StateView {
    fn tree_index(&mut self, _key: Bytes32) -> Option<u64> {
        unreachable!("VM forward run should not invoke the tree")
    }

    fn merkle_proof(&mut self, _tree_index: u64) -> LeafProof {
        unreachable!("VM forward run should not invoke the tree")
    }

    fn prev_tree_index(&mut self, _key: Bytes32) -> u64 {
        unreachable!("VM forward run should not invoke the tree")
    }
}

impl StateView {
    pub fn get_account(&mut self, address: B160) -> Option<AccountProperties> {
        let key = derive_flat_storage_key(
            &ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
            &address_into_special_storage_key(&address),
        );
        self.read(key).map(|hash| {
            AccountProperties::decode(&self.get_preimage(hash).unwrap().try_into().unwrap())
        })
    }
}
