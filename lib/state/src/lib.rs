mod metrics;
mod persistent_preimages;
pub mod persistent_storage_map;
pub mod storage_map;
mod storage_map_view;
mod storage_metrics;

use alloy::primitives::{B256, BlockNumber};
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;
use zksync_os_interface::traits::{PreimageSource, ReadStorage};
use zksync_os_interface::types::StorageWrite;
use zksync_os_rocksdb::RocksDB;
// Re-export commonly used types
use crate::persistent_preimages::{PersistentPreimages, PreimagesCF};
use crate::storage_map_view::StorageMapView;
pub use persistent_storage_map::{PersistentStorageMap, StorageMapCF};
pub use storage_map::{Diff, StorageMap};
use zksync_os_genesis::Genesis;
use zksync_os_storage_api::{ReadStateHistory, StateError, ViewState, WriteState};

const STATE_STORAGE_DB_NAME: &str = "state";
const PREIMAGES_STORAGE_DB_NAME: &str = "preimages";

const COMPACTING_DURATION: Duration = Duration::from_millis(100);

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
    pub async fn new(
        rocks_db_path: PathBuf,
        blocks_to_retain_in_memory: usize,
        genesis: &Genesis,
    ) -> Self {
        let state_db = RocksDB::<StorageMapCF>::new(&rocks_db_path.join(STATE_STORAGE_DB_NAME))
            .expect("Failed to open State DB");
        let persistent_storage_map = PersistentStorageMap::new(state_db, genesis).await;

        let storage_map = StorageMap::new(persistent_storage_map, blocks_to_retain_in_memory);

        let preimages_db =
            RocksDB::<PreimagesCF>::new(&rocks_db_path.join(PREIMAGES_STORAGE_DB_NAME))
                .expect("Failed to open Preimages DB");

        let persistent_preimages = PersistentPreimages::new(preimages_db, genesis).await;

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

    pub async fn compact_periodically(&self) {
        let mut ticker = tokio::time::interval(COMPACTING_DURATION);
        let map = self.storage_map.clone();
        // can take more than `period` to compact - use proper scheduler
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
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        self.preimages.get_preimage(hash)
    }
}
impl ReadStorage for StateView {
    fn read(&mut self, key: B256) -> Option<B256> {
        self.storage_map_view.read(key)
    }
}

impl ReadStateHistory for StateHandle {
    fn state_view_at(&self, block_number: BlockNumber) -> Result<impl ViewState, StateError> {
        Ok(StateView {
            storage_map_view: self.storage_map.view_at(block_number)?,
            preimages: self.persistent_preimages.clone(),
        })
    }

    fn block_range_available(&self) -> RangeInclusive<u64> {
        self.compacted_block_number()..=self.storage_map.latest_block.load(Ordering::Relaxed)
    }
}

impl WriteState for StateHandle {
    /// Adds a block result to the state components
    /// No atomicity guarantees
    /// PreimageType is currently ignored
    fn add_block_result<'a, J>(
        &self,
        block_number: u64,
        storage_diffs: Vec<StorageWrite>,
        new_preimages: J,
        // In-memory diffs are never expected to be overridden so this is ignored
        _override_allowed: bool,
    ) -> anyhow::Result<()>
    where
        J: IntoIterator<Item = (B256, &'a Vec<u8>)>,
    {
        self.storage_map.add_diff(block_number, storage_diffs);
        self.persistent_preimages.add(block_number, new_preimages);
        Ok(())
    }
}
