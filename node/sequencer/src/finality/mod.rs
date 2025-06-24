use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use zksync_os_state::{StateHandle, StateView};
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
            tracing::debug!("already canonized until {} - skipping block {}", cur_canonized, block_number);
        }
    }
    pub fn get_canonized_block(&self) -> u64 {
        self.canonized.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn canonized_state_guard(&self, state: StateHandle) -> CanonizedStateGuard {
        CanonizedStateGuard {
            _canonized: self.canonized.clone(),
            state,
        }
    }
}

#[derive(Debug)]
pub struct CanonizedStateGuard {
    _canonized: Arc<AtomicU64>,
    state: StateHandle,
}

impl CanonizedStateGuard {
    pub fn access_state(&self, block_number: u64) -> anyhow::Result<StateView> {
        // todo: restrict by block number
        self.state.state_view_at_block(block_number)
    }
}

// #[derive(Debug)]
// pub struct CanonizedRepositoriesGuard {
//     canonized: Arc<AtomicU64>,
//     repository_manager: RepositoryManager
// }
//
// impl CanonizedStateGuard {
//     pub fn access_block()
// }
