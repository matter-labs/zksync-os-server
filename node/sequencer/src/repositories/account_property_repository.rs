use alloy::primitives::{Address, B256};
use dashmap::DashMap;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use zk_ee::common_structs::PreimageType;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    AccountProperties, ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
};
use zk_os_forward_system::run::BatchOutput;

/// History-based repository for account properties with compaction.
///
/// Stores a mutable base state as of `base_block`, plus diffs for up to `blocks_to_retain` blocks
/// beyond the base. When more diffs are added, the oldest are merged into `base_state` and evicted.
///
/// todo: base state is not persisted - that means that upon restart some balances are inaccessible via RPC
///
/// todo: note that all account properties also exist in preiamages (state component).
///     So instead of storing them here in full, we should only store hashes here
///     and retrieve values from the preimages
#[derive(Clone, Debug)]
pub struct AccountPropertyRepository {
    /// The lowest block whose state lives entirely in `base_state`.
    base_block: Arc<AtomicU64>,
    /// Base properties at `base_block`.
    pub base_state: Arc<DashMap<Address, AccountProperties>>,
    /// Per-block snapshots: block_number → full AccountProperties map for that block.
    diffs: Arc<DashMap<u64, Arc<HashMap<Address, AccountProperties>>>>,
    /// How many diffs to retain before compaction.
    blocks_to_retain: usize,
}

impl AccountPropertyRepository {
    /// Create a new repository with the specified number of blocks to retain.
    pub fn new(blocks_to_retain: usize) -> Self {
        AccountPropertyRepository {
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
    pub fn add_diff(&self, block: u64, diff: HashMap<Address, AccountProperties>) {
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

    /// Read the `AccountProperties` for `addr` as of `block`.
    ///
    /// Scans per-block snapshots from `block - 1` down to `base_block + 1`. If none contains `addr`,
    /// falls back to `base_state` at `base_block`. Returns `None` if still not found.
    pub fn get_at_block(&self, block: u64, addr: &Address) -> Option<AccountProperties> {
        let base = self.base_block.load(Ordering::Relaxed);
        // Scan diffs newest-first
        for bn in (base + 1..=block).rev() {
            if let Some(diff_arc) = self.diffs.get(&bn) {
                if let Some(props) = diff_arc.get(addr) {
                    return Some(*props);
                }
            }
        }
        // Fallback to base_state
        self.base_state.get(addr).map(|r| *r.value())
    }

    fn latest_block(&self) -> u64 {
        let base = self.base_block.load(Ordering::Relaxed);

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
        // todo: we are adding `+1` because we want the state for the block (latest + 1)
        // alternatively the `latest_block` function itself could return `+1`
        let r = self.get_at_block(latest + 1, addr);

        tracing::debug!(
            latest_block=latest,
            nonce = r.map(|e| e.nonce),
            addr=?addr,
            "getting latest account properties",
        );

        r
    }
}

/// Extract account properties from a BatchOutput.
///
/// This method processes the published preimages and storage writes to extract
/// account properties that were updated during block execution.
pub fn extract_account_properties(
    block_output: &BatchOutput,
) -> (HashMap<Address, AccountProperties>, HashMap<B256, Vec<u8>>) {
    // First, collect all account properties from published preimages
    let mut account_properties_preimages = HashMap::new();
    let mut bytecodes = HashMap::new();
    for (hash, preimage, preimage_type) in &block_output.published_preimages {
        match preimage_type {
            PreimageType::Bytecode => {
                bytecodes.insert(B256::from(hash.as_u8_array()), preimage.clone());
            }
            PreimageType::AccountData => {
                account_properties_preimages.insert(
                    *hash,
                    AccountProperties::decode(
                        &preimage
                            .clone()
                            .try_into()
                            .expect("Preimage should be exactly 124 bytes"),
                    ),
                );
            }
        }
    }

    // Then, map storage writes to account addresses
    let mut result = HashMap::new();
    for log in &block_output.storage_writes {
        if log.account == ACCOUNT_PROPERTIES_STORAGE_ADDRESS {
            let account_address = Address::from_slice(&log.account_key.as_u8_array()[12..]);

            if let Some(properties) = account_properties_preimages.get(&log.value) {
                result.insert(account_address, *properties);
            } else {
                tracing::warn!(
                    "Account properties preimage not found for address {} and value {:?}",
                    account_address,
                    log.value
                );
                panic!();
            }
        }
    }

    (result, bytecodes)
}
