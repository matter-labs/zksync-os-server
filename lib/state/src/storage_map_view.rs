use std::sync::Arc;
use dashmap::DashMap;
use zk_ee::utils::Bytes32;
use zk_os_forward_system::run::ReadStorage;
use crate::{Diff, PersistentStorageMap};
use crate::metrics::STORAGE_VIEW_METRICS;

/// Storage View valid for a specific block (`block`)
/// It represents the state for the beginning of block `block`.
#[derive(Debug, Clone)]
pub struct StorageMapView {
    /// Block number for which this view is valid.
    pub block: u64,
    /// Block preceding the first block in diffs
    /// note: it's possible that persistence will be compacted for blocks after `base_block`
    /// and diffs removed from memory - but that's OK - as long as diffs around `block` are not compacted
    // todo: in fact we could infer this from `diffs` - by iterating backwards until the first missing element
    pub base_block: u64,
    /// All diffs after `base_block` and before `block`
    pub diffs: Arc<DashMap<u64, Arc<Diff>>>,
    /// fallback persistence for cases when value is not in diffs
    pub persistent_storage_map: PersistentStorageMap,
}



impl ReadStorage for StorageMapView {
    /// Reads `key` by scanning block diffs from `block - 1` down to `base_block + 1`,
    /// then falling back to the persistence
    fn read(&mut self, key: Bytes32) -> Option<Bytes32> {
        let latency_diffs = STORAGE_VIEW_METRICS.access[&"diff"].start();
        let latency_total = STORAGE_VIEW_METRICS.access[&"total"].start();

        for bn in (self.base_block + 1..self.block).rev() {
            if let Some(diff) = self.diffs.get(&bn) {
                let res = diff.map.get(&key);
                if let Some(value) = res {
                    latency_diffs.observe();
                    latency_total.observe();
                    STORAGE_VIEW_METRICS
                        .diffs_scanned
                        .observe(self.block - bn);
                    return Some(*value);
                }
            } else {
                tracing::debug!(
                    "StorageMapView for {} (base block {}) read key: no diff found for block {}",
                    self.block,
                    self.base_block,
                    bn
                );
                // this means this diff is compacted - and so are the diffs before - no point in continuing iteration
                // this is fine as long as the compaction target is below `self.block`.
                // This is currently not checked - but assumed to be true as long as storage views are short-lived
                // todo: add this check
                break;
            }
        }

        latency_diffs.observe();
        STORAGE_VIEW_METRICS
            .diffs_scanned
            .observe(self.block - self.base_block);

        // Fallback to base_state
        let latency_base = STORAGE_VIEW_METRICS.access[&"base"].start();
        let r = self.persistent_storage_map.get(key);
        latency_base.observe();

        latency_total.observe();
        r
    }
}