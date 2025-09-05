use crate::metrics::STORAGE_MAP_METRICS;
use crate::persistent_storage_map::PersistentStorageMap;
use crate::storage_map_view::StorageMapView;
use dashmap::DashMap;
use std::sync::atomic::AtomicU64;
use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
};
use zksync_os_interface::bytes32::Bytes32;
use zksync_os_interface::common_types::StorageWrite;
use zksync_os_storage_api::{StateError, StateResult};

#[derive(Debug, Clone)]
pub struct StorageMap {
    /// latest block guaranteed to present in `diffs`
    /// This allows for StorageMapView creation up to block `latest_block + 1`
    pub latest_block: Arc<AtomicU64>,

    /// block → key → value map. Latest ~`blocks_to_retain` items
    /// invariants:
    ///  * Contiguous
    ///  * Max number >= `latest_block` (unless empty)
    ///  * Empty on start. Grows to at least `blocks_to_retain` items
    ///  * Only compacts when more than `blocks_to_retain` items exist

    ///    Note: currently `state` crate doesn't know anything about canonization,
    ///    so it may compact non-canonized block if there is a significant delay in canonization,
    ///    this should not happen in practice because of canonization back pressure
    ///    (block tip should not be ahead of the last canonized block by more than `blocks_to_retain` blocks)
    pub diffs: Arc<DashMap<u64, Arc<Diff>>>,

    /// RocksDB handle for the persistent base - cheap to clone
    pub persistent_storage_map: PersistentStorageMap,

    /// Configuration option. We always have at least `blocks_to_retain` elements in `diffs`
    pub blocks_to_retain: usize,
}

#[derive(Debug)]
pub struct Diff {
    pub map: HashMap<Bytes32, Bytes32>,
}

impl Diff {
    fn new(writes: Vec<StorageWrite>) -> Self {
        Self {
            map: writes.into_iter().map(|w| (w.key, w.value)).collect(),
        }
    }
}

impl StorageMap {
    pub fn view_at(&self, block_number: u64) -> StateResult<StorageMapView> {
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
        // because view_at(N) should return view immediately after block N
        if block_number < persistent_block_upper_bound {
            return Err(StateError::Compacted(block_number));
        }

        if block_number > latest_block {
            return Err(StateError::NotFound(block_number));
        }

        Ok(StorageMapView {
            block: block_number,
            // it's important to use lower_bound here since later blocks are not guaranteed to be in rocksDB yet
            base_block: persistent_block_lower_bound,
            diffs: self.diffs.clone(),
            persistent_storage_map: self.persistent_storage_map.clone(),
        })
    }

    pub fn new(persistent_storage_map: PersistentStorageMap, blocks_to_retain: usize) -> Self {
        let rocksdb_block = persistent_storage_map.rocksdb_block_number();

        tracing::info!("Initializing with RocksDB at: {}", rocksdb_block);

        Self {
            latest_block: Arc::new(AtomicU64::new(rocksdb_block)),
            diffs: Arc::new(DashMap::new()),
            persistent_storage_map,
            blocks_to_retain,
        }
    }

    /// Adds a diff for block `block` (thus providing state for `block + 1`)
    /// Must be contiguous - that is, can only add blocks in order
    pub fn add_diff(&self, block_number: u64, writes: Vec<StorageWrite>) {
        let total_latency_observer = STORAGE_MAP_METRICS.add_diff.start();

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
                panic!("missing diff for block {block_number} - latest_memory_block is {latest_memory_block}")
            });

            // Temporary:
            // check that we are inserting block with the same data.
            // Doesn't need to hold true with decentralization (ie actual rollbacks)
            // Clones are expensive but only happen for bounded number of blocks at startup
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
        total_latency_observer.observe();
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
                panic!("No diff found for block {block_number} while compacting");
            }
        }
    }

    /// Aggregates all key-value updates between `from` and `to` (inclusive),
    /// returning the last written value for each key
    pub fn collect_diffs_range(
        &self,
        from: u64,
        to: u64,
    ) -> anyhow::Result<HashMap<Bytes32, Bytes32>> {
        let mut aggregated_map = HashMap::new();

        for block_number in from..=to {
            match self.diffs.get(&block_number) {
                Some(diff) => aggregated_map.extend(diff.value().map.iter()),
                None => {
                    return Err(anyhow::anyhow!(
                        "StorageMap: compacting diffs, but no diff found for block {}",
                        block_number
                    ));
                }
            }
        }

        Ok(aggregated_map)
    }
}
