// account_properties_history.rs

use dashmap::DashMap;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use zk_os_basic_system::system_implementation::flat_storage_model::AccountProperties;
use zksync_types::Address;

/// History of account properties per block, with compaction.
///
/// Stores a mutable base state as of `base_block`, plus diffs for up to `max_entries` blocks
/// beyond the base. When more diffs are added, the oldest are merged into `base_state` and evicted.
///
#[derive(Clone, Debug)]
pub struct InMemoryAccountProperties {
    /// The lowest block whose state lives entirely in `base_state`.
    base_block: Arc<AtomicU64>,
    /// Base properties at `base_block`.
    pub base_state: Arc<DashMap<Address, AccountProperties>>,
    /// Per-block snapshots: block_number → full AccountProperties map for that block.
    diffs: Arc<DashMap<u64, Arc<HashMap<Address, AccountProperties>>>>,
    /// How many diffs to retain before compaction.
    blocks_to_retain: usize,
}

impl InMemoryAccountProperties {
    /// Create a new history.
    ///
    /// `initial_block` is the block at which `base_state_map` is valid.
    /// `max_entries` is the maximum number of per-block diffs to keep.
    pub fn empty(blocks_to_retain: usize) -> Self {
        InMemoryAccountProperties {
            base_block: Arc::new(AtomicU64::new(0)),
            base_state: Arc::new(Default::default()),
            diffs: Arc::new(DashMap::new()),
            blocks_to_retain,
        }
    }

    /// Insert the full account properties map for `block`.
    ///
    /// The caller should invoke this for blocks in strictly ascending order (i.e.
    /// each `block` is `> latest_block()`).
    /// After insertion, evicts the oldest diff if the number of diffs exceeds `max_entries`.
    pub fn add_diff(&self, block: u64, diff: HashMap<Address, AccountProperties>) {
        // tracing::info!("Adding account properties diff for block {}: {:?}", block, diff);

        // Insert the new diff
        self.diffs.insert(block, Arc::new(diff));

        // Compact if too many diffs
        while self.diffs.len() > self.blocks_to_retain {
            // Find the oldest block key
            if let Some((oldest_block, _)) = self
                .diffs
                .iter()
                .map(|e| (e.key().clone(), e.value().clone()))
                .min_by_key(|&(blk, _)| blk)
            {
                // Remove and merge into base_state
                if let Some((_, old_diff_arc)) = self.diffs.remove(&oldest_block) {
                    // Try to unwrap the Arc to avoid clone; otherwise clone the map
                    let old_map = match Arc::try_unwrap(old_diff_arc) {
                        Ok(old_map) => old_map,
                        Err(arc) => (*arc).clone(),
                    };
                    // Merge each entry into base_state
                    for (addr, props) in old_map {
                        self.base_state.insert(addr, props);
                    }
                    // Advance base_block
                    self.base_block.store(oldest_block, Ordering::SeqCst);
                }
            } else {
                break;
            }
        }
    }

    /// Read the `AccountProperties` for `addr` as of `block`.
    ///
    /// Scans per-block snapshots from `block` down to `base_block + 1`. If none contains `addr`,
    /// falls back to `base_state` at `base_block`. Returns `None` if still not found.
    pub fn get(&self, block: u64, addr: &Address) -> Option<AccountProperties> {
        let base = self.base_block.load(Ordering::SeqCst);
        // Scan diffs newest-first
        for bn in (base + 1..=block).rev() {
            if let Some(diff_arc) = self.diffs.get(&bn) {
                if let Some(props) = diff_arc.get(&addr) {
                    let res = props.clone();
                    // tracing::info!("Found account properties for {:?} at block {}: {:?}",
                    //     addr,
                    //     bn,
                    //     props
                    // );
                    return Some(res);
                }
            }
        }
        // Fallback to base_state
        let res = self.base_state.get(&addr).map(|r| r.value().clone());

        // tracing::info!("Account properties for {:?} at block {} not found in diffs, falling back to base state: {:?}",
        //     addr,
        //     block,
        //     res
        // );
        res
    }

    fn latest_block(&self) -> u64 {
        let base = self.base_block.load(Ordering::SeqCst);

        // max over all known diff keys; if `diffs` is empty, fall back to base
        self.diffs
            .iter()
            .map(|e| *e.key())
            .fold(base, |acc, b| acc.max(b))
    }

    /// Read `AccountProperties` for `addr` at the latest known block,
    /// falling back to the consolidated base state.
    pub fn get_latest(&self, addr: &Address) -> Option<AccountProperties> {
        let latest = self.latest_block();
        let res = self.get(latest, addr);

        // tracing::info!("Getting account properties for {:?} at latest block {}: {:?}",
        //     address_to_bytes32(addr),
        //     latest,
        //     res
        // );
        res
    }
}
