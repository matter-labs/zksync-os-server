use dashmap::DashMap;
use std::sync::Arc;
use zk_os_forward_system::run::BatchOutput;

/// In-memory repository of the most recent N `BatchOutput`s, keyed by block number.
///
/// Inserts must happen in strictly ascending order.
///
#[derive(Clone, Debug)]
pub struct BlockReceiptRepository {
    /// Maximum number of entries to retain.
    blocks_to_retain: usize,
    /// Map from block number → `BatchOutput`.
    receipts: Arc<DashMap<u64, BatchOutput>>,
}

impl BlockReceiptRepository {
    /// Create a new repository retaining up to `blocks_to_retain` items.
    pub fn new(blocks_to_retain: usize) -> Self {
        BlockReceiptRepository {
            blocks_to_retain,
            receipts: Arc::new(DashMap::new()),
        }
    }

    /// Insert the `BatchOutput` for `block`.
    ///
    /// Must be called with `block == latest_block() + 1`. Evicts the
    /// oldest entry if we exceed `blocks_to_retain`.
    pub fn insert(&self, block: u64, output: BatchOutput) {
        // Store the new receipt
        self.receipts.insert(block, output);
        // Evict the oldest if over capacity
        // todo: assumes asceding order and no gaps - it holds, but is error-prone - rewrite
        // (consider storing bounds explicitly or infer from `receipts` map)
        if block > self.blocks_to_retain as u64 {
            let to_evict = block - self.blocks_to_retain as u64;
            self.receipts.remove(&to_evict);
        }
    }

    /// Retrieve the `BatchOutput` for `block`, if still retained.
    pub fn get_by_block(&self, block: u64) -> Option<BatchOutput> {
        self.receipts.get(&block).map(|r| r.value().clone())
    }
}
