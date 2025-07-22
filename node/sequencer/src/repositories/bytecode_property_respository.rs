use alloy::primitives::B256;
use dashmap::DashMap;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// History-based repository for bytecodes with compaction.
///
/// Stores a mutable base state as of `base_block`, plus diffs for up to `blocks_to_retain` blocks
/// beyond the base. When more diffs are added, the oldest are merged into `base_state` and evicted.
///
/// todo: base state is not persisted - that means that upon restart some balances are inaccessible via RPC
///
/// todo: note that all bytecodes also exist in preiamages (state component).
///     So instead of storing them here in full, we should only store hashes here
///     and retrieve values from the preimages
#[derive(Clone, Debug)]
pub struct BytecodeRepository {
    /// The lowest block whose state lives entirely in `base_state`.
    base_block: Arc<AtomicU64>,
    /// Base bytecodes at `base_block`.
    pub base_state: Arc<DashMap<B256, Vec<u8>>>,
    /// Per-block snapshots: block_number → full bytecodes map for that block.
    #[allow(clippy::type_complexity)]
    diffs: Arc<DashMap<u64, Arc<HashMap<B256, Vec<u8>>>>>,
    /// How many diffs to retain before compaction.
    blocks_to_retain: usize,
}

impl BytecodeRepository {
    /// Create a new repository with the specified number of blocks to retain.
    pub fn new(blocks_to_retain: usize) -> Self {
        BytecodeRepository {
            base_block: Arc::new(AtomicU64::new(0)),
            base_state: Arc::new(Default::default()),
            diffs: Arc::new(DashMap::new()),
            blocks_to_retain,
        }
    }

    /// Insert the full account properties map for `block`.
    ///
    /// The caller should invoke this for blocks in strictly ascending order (i.e.
    /// each `block` = `latest_block() + 1`).
    /// After insertion, evicts the oldest diff if the number of diffs exceeds `blocks_to_retain`.
    pub fn add_diff(&self, block: u64, diff: HashMap<B256, Vec<u8>>) {
        // Insert the new diff
        self.diffs.insert(block, Arc::new(diff));

        // Compact if too many diffs
        // todo - instead of a `while`, do `for` - we know the exact numbers we need to compact
        // (LLM generated code)
        while self.diffs.len() > self.blocks_to_retain {
            // Find the oldest block key
            if let Some((oldest_block, _)) = self
                .diffs
                .iter()
                .map(|e| (*e.key(), e.value().clone()))
                .min_by_key(|&(blk, _)| blk)
            {
                // Remove and merge into base_state
                if let Some((_, old_diff_arc)) = self.diffs.remove(&oldest_block) {
                    // Try to unwrap the Arc to avoid clone; otherwise clone the map
                    let old_map =
                        Arc::try_unwrap(old_diff_arc).unwrap_or_else(|arc| (*arc).clone());
                    // Merge each entry into base_state
                    for (addr, props) in old_map {
                        self.base_state.insert(addr, props);
                    }
                    // Advance base_block
                    self.base_block.store(oldest_block, Ordering::Relaxed);
                }
            } else {
                break;
            }
        }
    }

    /// Read the bytecode for given hash as of `block` (inclusive).
    ///
    /// Scans per-block snapshots from `block` down to `base_block + 1`. If none contains `addr`,
    /// falls back to `base_state` at `base_block`. Returns `None` if still not found.
    pub fn get_at_block(&self, block: u64, hash: &B256) -> Option<Vec<u8>> {
        let base = self.base_block.load(Ordering::Relaxed);
        // Scan diffs newest-first
        for bn in (base + 1..=block).rev() {
            if let Some(diff_arc) = self.diffs.get(&bn) {
                if let Some(bytecode) = diff_arc.get(hash) {
                    return Some(bytecode.clone());
                }
            }
        }
        // Fallback to base_state
        self.base_state.get(hash).map(|r| r.value().clone())
    }

    fn latest_block(&self) -> u64 {
        let base = self.base_block.load(Ordering::Relaxed);

        // max over all known diff keys; if `diffs` is empty, fall back to base
        self.diffs
            .iter()
            .map(|e| *e.key())
            .fold(base, |acc, b| acc.max(b))
    }

    /// Read bytecode for given hash at the latest known block,
    /// falling back to the consolidated base state.
    pub fn get_latest(&self, hash: &B256) -> Option<Vec<u8>> {
        let latest = self.latest_block();
        // todo: we are adding `+1` because we want the state for the block (latest + 1)
        // alternatively the `latest_block` function itself could return `+1`
        let r = self.get_at_block(latest + 1, hash);

        tracing::debug!(
            latest_block = latest,
            bytecode_len = r.as_ref().map(|e| e.len()),
            ?hash,
            "getting latest bytecode",
        );

        r
    }
}
