//! An in-memory [`ExecutionEnv`] for simulation.
//!
//! Stands where the node's real execution will stand: builds child blocks on request,
//! verifies proposals, and keeps a committed chain. Because every validator's committed
//! chain is observable, tests can assert the property that actually matters — all
//! honest validators commit the identical sequence of blocks.

use crate::block::SimBlock;
use commonware_consensus::types::Height;
use commonware_cryptography::Digestible;
use commonware_runtime::{Clock as _, deterministic};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zksync_os_consensus_core::{BuildContext, ExecutionEnv};

/// An [`ExecutionEnv`] that the simulated cluster can observe: what has this validator
/// durably committed? Any execution backend (mock or real) implements this so the same
/// cluster harness and assertions work over both.
pub trait SimEnv: ExecutionEnv {
    /// Height of the last committed block, if any.
    fn committed_tip(&self) -> Option<u64>;

    /// Digests of the committed chain, in height order. Digests (rather than blocks)
    /// are what agreement assertions compare — two validators committed "the same
    /// chain" exactly when their digest sequences match.
    fn committed_chain_digests(&self) -> Vec<<Self::Block as Digestible>::Digest>;
}

/// Shared-state mock execution: clones observe and mutate the same chain, exactly like
/// clones of a real execution handle would.
#[derive(Clone, Default)]
pub struct MockExecution {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// The committed chain, starting at height 1 (genesis is implicit at height 0).
    committed: Vec<SimBlock>,
}

impl MockExecution {
    pub fn new() -> Self {
        Self::default()
    }

    /// The committed chain so far (test probe).
    pub fn committed_chain(&self) -> Vec<SimBlock> {
        self.inner.lock().unwrap().committed.clone()
    }

    /// Height of the last committed block, if any (test probe).
    pub fn committed_tip(&self) -> Option<u64> {
        let inner = self.inner.lock().unwrap();
        inner.committed.last().map(|block| {
            use commonware_consensus::Heightable;
            block.height().get()
        })
    }
}

impl SimEnv for MockExecution {
    fn committed_tip(&self) -> Option<u64> {
        MockExecution::committed_tip(self)
    }

    fn committed_chain_digests(&self) -> Vec<commonware_cryptography::sha256::Digest> {
        self.committed_chain()
            .iter()
            .map(|block| block.digest())
            .collect()
    }
}

impl ExecutionEnv for MockExecution {
    type Block = SimBlock;

    async fn genesis_block(&mut self) -> SimBlock {
        SimBlock::genesis()
    }

    async fn build(&mut self, parent: SimBlock, context: BuildContext) -> Option<SimBlock> {
        // Seeding content with the view makes re-proposals distinguishable: if the block
        // built in view 7 is abandoned and a new leader builds on the same parent in
        // view 8, the two blocks differ — like real blocks built at different times.
        Some(SimBlock::child_of(&parent, context.view))
    }

    async fn verify(&mut self, parent: SimBlock, block: SimBlock) -> bool {
        use commonware_consensus::{Block, Heightable};
        // Consensus already checked the structural linkage before calling us; this
        // re-check stands in for real content verification (which would re-execute the
        // block and compare outputs).
        block.parent() == parent.digest() && block.height().get() == parent.height().get() + 1
    }

    async fn committed_height(&mut self) -> Option<Height> {
        self.committed_tip().map(Height::new)
    }

    async fn commit(&mut self, block: SimBlock) {
        use commonware_consensus::Heightable;
        let mut inner = self.inner.lock().unwrap();
        let next_height = inner.committed.len() as u64 + 1;
        let height = block.height().get();
        if height < next_height {
            // At-least-once delivery: after a restart, consensus replays blocks the node
            // already has. They must be the *same* blocks — a mismatch here would mean
            // two conflicting blocks were finalized at one height, the one thing BFT
            // consensus exists to prevent.
            let existing = &inner.committed[(height - 1) as usize];
            assert_eq!(
                existing, &block,
                "re-committed block at height {height} differs from the committed one"
            );
            return;
        }
        assert_eq!(
            height, next_height,
            "commit out of order: got height {height}, expected {next_height}",
        );
        inner.committed.push(block);
    }
}

/// Wraps an execution environment, delaying chosen operations by virtual time — the
/// deterministic stand-in for a validator whose execution is slow (long `verify`) or
/// whose persistence lags (long `commit`). Everything else delegates unchanged.
#[derive(Clone)]
pub struct DelayedEnv<X> {
    inner: X,
    context: deterministic::Context,
    verify_delay: Duration,
    commit_delay: Duration,
}

impl<X: SimEnv> DelayedEnv<X> {
    pub fn slow_verify(inner: X, context: deterministic::Context, delay: Duration) -> Self {
        Self {
            inner,
            context,
            verify_delay: delay,
            commit_delay: Duration::ZERO,
        }
    }

    pub fn slow_commit(inner: X, context: deterministic::Context, delay: Duration) -> Self {
        Self {
            inner,
            context,
            verify_delay: Duration::ZERO,
            commit_delay: delay,
        }
    }
}

impl<X: SimEnv> ExecutionEnv for DelayedEnv<X> {
    type Block = X::Block;

    async fn genesis_block(&mut self) -> Self::Block {
        self.inner.genesis_block().await
    }

    async fn build(&mut self, parent: Self::Block, context: BuildContext) -> Option<Self::Block> {
        self.inner.build(parent, context).await
    }

    async fn verify(&mut self, parent: Self::Block, block: Self::Block) -> bool {
        if !self.verify_delay.is_zero() {
            self.context.sleep(self.verify_delay).await;
        }
        self.inner.verify(parent, block).await
    }

    async fn has_state(&mut self, block: &Self::Block) -> bool {
        self.inner.has_state(block).await
    }

    async fn committed_height(&mut self) -> Option<Height> {
        self.inner.committed_height().await
    }

    async fn adopt_committed_block(&mut self, block: &Self::Block) {
        self.inner.adopt_committed_block(block).await
    }

    async fn commit(&mut self, block: Self::Block) {
        if !self.commit_delay.is_zero() {
            self.context.sleep(self.commit_delay).await;
        }
        self.inner.commit(block).await
    }
}

impl<X: SimEnv> SimEnv for DelayedEnv<X> {
    fn committed_tip(&self) -> Option<u64> {
        self.inner.committed_tip()
    }

    fn committed_chain_digests(&self) -> Vec<<Self::Block as Digestible>::Digest> {
        self.inner.committed_chain_digests()
    }
}
