use crate::persistent_preimages::PreimagesCF;
use crate::{StateHandle, StorageMapCF};
use std::collections::HashSet;
use zk_ee::utils::Bytes32;

#[derive(Debug)]
#[allow(dead_code)]
pub struct StorageMetrics {
    /// total storage keys in memory (potentially duplicated/rewritten across blocks)
    pub storage_keys: usize,
    /// Distinct storage keys across all hot diffs.
    pub storage_unique_keys: usize,
    /// Number of diff blocks in RAM.
    pub diff_blocks: usize,
    /// Estimated key-value pairs persisted in RocksDB.
    pub rocksdb_storage_entries: u64,
    /// Estimated key-value pairs persisted in RocksDB.
    pub rocksdb_preimages_entries: u64,
}

impl StorageMetrics {
    /// consider delegating individual
    pub fn collect_metrics(state: StateHandle) -> StorageMetrics {
        let mut unique: HashSet<Bytes32> = HashSet::new();
        let mut storage_keys = 0usize;

        for entry in state.storage_map.diffs.iter() {
            let diff = entry.value(); // &Arc<HashMap<..>>
            storage_keys += diff.map.len();
            unique.extend(diff.map.keys().copied()); // &Bytes32 → Bytes32
        }

        let rocksdb_storage_entries = state
            .storage_map
            .persistent_storage_map
            .rocks
            .estimated_number_of_entries(StorageMapCF::Storage);

        let rocksdb_preimages_entries = state
            .persistent_preimages
            .rocks
            .estimated_number_of_entries(PreimagesCF::Storage);

        StorageMetrics {
            storage_keys,
            storage_unique_keys: unique.len(),
            diff_blocks: state.storage_map.diffs.len(),
            rocksdb_storage_entries,
            rocksdb_preimages_entries,
        }
    }
}
