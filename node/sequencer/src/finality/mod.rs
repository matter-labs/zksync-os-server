use std::sync::atomic::AtomicU64;
use std::sync::Arc;
// todo: experimental approach - reconsider after adding more finality stages

/// Holds block numbers for various finality stages
///
#[derive(Debug, Clone)]
pub struct FinalityTracker {
    /// the latest canonized block number
    /// for a decentralized case this means accepted and signed by the validator quorum
    /// for a centralized case this is equivalent to durability - i.e., block is written to WAL
    canonized: Arc<AtomicU64>,
    // the latest block present in state and all repositories - not necessarily canonized yet.
    // sealed: Arc<AtomicU64>,
}

impl FinalityTracker {
    pub fn new(initial_canonized_block: u64) -> Self {
        Self {
            canonized: Arc::new(initial_canonized_block.into()),
            // sealed: Arc::new(initial_canonized_block.into()),
        }
    }

    pub fn advance_canonized(&self, block_number: u64) {
        let cur_canonized = self.get_canonized_block();
        assert!(
            block_number <= cur_canonized + 1,
            "cannot have gaps when advancing canonized"
        );
        if cur_canonized < block_number {
            self.canonized
                .store(block_number, std::sync::atomic::Ordering::Relaxed);
        } else {
            tracing::debug!(
                "already canonized until {} - skipping block {}",
                cur_canonized,
                block_number
            );
        }
    }
    pub fn get_canonized_block(&self) -> u64 {
        self.canonized.load(std::sync::atomic::Ordering::Relaxed)
    }
}
