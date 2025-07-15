use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::{
    LeafProof, PreimageSource, ReadStorage, ReadStorageTree, StorageWrite,
};
use zksync_storage::RocksDB;

pub mod config;
mod metrics;
mod persistent_preimages;
mod persistent_storage_map;
mod state_view;
mod storage_metrics;

// Re-export commonly used types
use crate::persistent_preimages::{PersistentPreimages, PreimagesCF};
use crate::state_view::StorageMapView;
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
    persistent_storage_map: PersistentStorageMap,
    persistent_preimages: PersistentPreimages,
    pub cur_block: Arc<AtomicU64>,
    pub last_checkpoint: Arc<AtomicU64>,
    pub checkpoints_to_retain: usize,
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

        let storage_map = StorageMap::new(persistent_storage_map, config.checkpoints_to_retain);

        let preimages_db =
            RocksDB::<PreimagesCF>::new(&config.rocks_db_path.join(PREIMAGES_STORAGE_DB_NAME))
                .expect("Failed to open Preimages DB");

        let persistent_preimages = PersistentPreimages::new(preimages_db);

        let storage_map_block = storage_map.latest_block.load(Ordering::Relaxed);
        let rocksdb_block = persistent_storage_map.rocksdb_block_number();
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

    /// Returns (state_block_number, preimages_block_number)
    ///
    /// Hint: we can return `min` of these two values
    /// for all intents and purposes - only returning both values for easier logging/context
    pub fn latest_block_numbers(&self) -> (u64, u64) {
        (
            self.storage_map.latest_block.load(Ordering::Relaxed),
            self.persistent_preimages.rocksdb_block_number(),
        )
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
        // can take more than `period` to compact - use proper scheduler
        loop {
            ticker.tick().await;
            map.compact();
        }
    }

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

    /// Adds a diff for block `block` (thus providing state for `block + 1`)
    /// Must be contiguous - that is, can only add blocks in order
    pub fn add_diff(&self, block_number: u64, writes: Vec<StorageWrite>) {
        let started_at = STORAGE_MAP_METRICS.add_diff.start();

        let latest_memory_block = self.latest_block.load(Ordering::Relaxed);

        assert!(
            block_number <= latest_memory_block + 1,
            "StorageMap: attempt to add block number {} - previous block is {}. Cannot have gaps in block data",
            block_number,
            latest_memory_block + 1
        );

        let new_diff = Diff::new(writes);
        if block_number == latest_memory_block + 1 {
            // normal case - inserting next block
            self.diffs.insert(block_number, Arc::new(new_diff));
        } else {
            // transaction replay or rollback
            let old_diff = self.diffs.get(&block_number).unwrap_or_else(|| {
                panic!(
                    "missing diff for block {} - latest_memory_block is be {}",
                    block_number, latest_memory_block,
                )
            });

            // Temporary:
            // check that we are inserting block with the same data.
            // Doesn't need to hold true with decentralization (ie actual rollbacks)
            // Clones are expensive but only happen fo bounded number of blocks at startup
            assert_eq!(
                old_diff.map.len(),
                new_diff.map.len(),
                "mismatch when replaying blocks"
            );
            for (old_k, old_v) in old_diff.map.clone() {
                assert_eq!(
                    old_v, new_diff.map[&old_k],
                    "mismatch when replaying blocks"
                );
            }

            // currently no-op as we don't allow changes
            self.diffs.insert(block_number, Arc::new(new_diff));
        }
        self.latest_block.store(block_number, Ordering::Relaxed);
        started_at.observe();
    }

    /// Moves elements from `diffs` to the persistence
    /// Only acts if there are more than `blocks_to_retain` blocks in memory
    pub fn compact(&self) {
        let latest_block = self.latest_block.load(Ordering::Relaxed);
        let compacting_until = latest_block.saturating_sub(self.blocks_to_retain as u64);

        let initial_persistent_block_upper_bound =
            self.persistent_storage_map.persistent_block_upper_bound();

        if compacting_until <= initial_persistent_block_upper_bound {
            // no-op
            tracing::debug!(
                "can_compact_until: {}, last_persisted: {}",
                compacting_until,
                initial_persistent_block_upper_bound,
            );
            return;
        }

        let compacted_diffs_to_compact = self
            .collect_diffs_range(initial_persistent_block_upper_bound + 1, compacting_until)
            .expect("cannot compact block range: one of the diffs is missing");

        tracing::info!(
            can_compact_until = compacting_until,
            initial_persistent_block_upper_bound = initial_persistent_block_upper_bound,
            "compacting {} blocks with {} unique keys",
            compacting_until - initial_persistent_block_upper_bound,
            compacted_diffs_to_compact.len()
        );

        self.persistent_storage_map
            .compact_sync(compacting_until, compacted_diffs_to_compact);

        for block_number in (initial_persistent_block_upper_bound + 1..=compacting_until).rev() {
            // todo: what will happen if there is a StorageMapView holding a reference to this diff?
            // todo: consider `try_unwrap`
            if let Some(_diff) = self.diffs.remove(&block_number) {
                tracing::debug!("Compacted diff for block {}", block_number);
            } else {
                panic!("No diff found for block {} while compacting", block_number);
            }
        }
    }
}
