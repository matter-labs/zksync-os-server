use dashmap::DashMap;
use std::sync::Arc;
use zk_os_forward_system::run::BatchOutput;

/// In-memory store of the most recent N `BatchOutput`s, keyed by block number.
///
/// Assumes that inserts happen strictly in increasing block order (always
/// for `latest_block + 1`), so eviction can be done by block arithmetic.
#[derive(Clone, Debug)]
pub struct InMemoryBlockReceipts {
    /// Maximum number of entries to retain.
    blocks_to_retain: usize,
    /// Map from block number → `BatchOutput`.
    receipts: Arc<DashMap<u64, BatchOutput>>,
}

impl InMemoryBlockReceipts {
    /// Create a new in-memory receipts store retaining up to `capacity` items.
    pub fn empty(blocks_to_retain: usize) -> Self {
        InMemoryBlockReceipts {
            blocks_to_retain,
            receipts: Arc::new(DashMap::new()),
        }
    }

    /// Insert the `BatchOutput` for `block`.
    ///
    /// Must be called with `block == latest_block() + 1`. Evicts the
    /// oldest entry if we exceed `capacity`.
    pub fn insert(&self, block: u64, output: BatchOutput) {
        // Store the new receipt
        self.receipts.insert(block, output);
        // Evict the oldest if over capacity
        if block > self.blocks_to_retain as u64 {
            let to_evict = block - self.blocks_to_retain as u64;
            self.receipts.remove(&to_evict);
        }
    }

    /// Retrieve the `BatchOutput` for `block`, if still retained.
    pub fn get(&self, block: u64) -> Option<BatchOutput> {
        self.receipts.get(&block).map(|r| r.value().clone())
    }

    /// Number of receipts currently stored.
    pub fn len(&self) -> usize {
        self.receipts.len()
    }
}
