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
    /// The chain height this environment's era is anchored at (0 unless the
    /// scenario models a migration). Decoded blocks carry it via the codec config
    /// so their consensus-side heights are era-relative.
    fn era_anchor(&self) -> u64 {
        0
    }

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
    /// Height the chain is anchored at: 0 for a chain that runs consensus from its
    /// genesis, the cutover height for a chain migrated from pre-consensus history
    /// (which this mock summarizes as just the anchor — content-free, like the rest
    /// of it).
    anchor_height: u64,
    /// The committed consensus-era chain: entry `i` has height `anchor_height + i + 1`.
    committed: Vec<SimBlock>,
    /// Idle behavior, when a scenario opts in; `None` = the mock always builds
    /// (every leader turn produces a block, the pre-idle-policy behavior).
    idle: Option<IdleWork>,
}

/// The mock's mempool stand-in for exercising [`IdlePolicy`] against real stack
/// dynamics: one *shared* pool of pending "work" units per cluster (transactions
/// gossip everywhere, so all real mempools converge on the same content). A
/// leader turn consumes a unit when it builds — the moment a real builder would
/// stream the transaction into its block — and with none pending consults the
/// policy exactly like the node builder's idle branch does.
#[derive(Clone)]
pub struct IdleWork(Arc<Mutex<IdleShared>>);

struct IdleShared {
    policy: zksync_os_consensus_core::idle_policy::IdlePolicy,
    /// Units of pending work; each built block consumes one.
    pending_work: u64,
    /// Heights whose unit is already spent. Consensus may abandon a built
    /// block (a nullified view) and ask a later leader to rebuild the same
    /// height; the real pool re-offers a transaction until a block carrying
    /// it *commits*, so the rebuild must carry the same work — not spend a
    /// second unit, and not (once units run dry) fall through to the policy
    /// and quietly drop the work altogether. Found by the proptest sweep:
    /// under a decline-only policy a nullified work block stalled its burst's
    /// wait forever.
    work_heights: std::collections::HashSet<u64>,
    /// Virtual-clock seconds each height was first built at. The production
    /// policy reads the *parent block's timestamp* — chain data, so a freshly
    /// built (even not-yet-committed) parent already reads as fresh and the
    /// next pipelined view declines. SimBlock carries no timestamp (its
    /// encoding is fingerprint-pinned), so this map stands in for it.
    built_at: std::collections::HashMap<u64, u64>,
    /// Fallback for heights this pool never saw built (the anchor, or blocks
    /// learned via backfill): the pool's construction time, then bumped at
    /// commits.
    last_progress: u64,
    /// The deterministic runtime's clock, captured as a closure so the mock
    /// stays non-generic.
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl IdleWork {
    pub fn new(
        policy: zksync_os_consensus_core::idle_policy::IdlePolicy,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        let start = now();
        Self(Arc::new(Mutex::new(IdleShared {
            policy,
            pending_work: 0,
            work_heights: std::collections::HashSet::new(),
            built_at: std::collections::HashMap::new(),
            last_progress: start,
            now,
        })))
    }

    /// Enqueues `n` units of work (the cluster's transactions); each unit makes
    /// one leader turn build a block.
    pub fn enqueue(&self, n: u64) {
        self.0.lock().unwrap().pending_work += n;
    }
}

impl MockExecution {
    pub fn new() -> Self {
        Self::default()
    }

    /// The sim side of the disaster-fork truncation: discards every committed
    /// block above `height` and re-anchors the environment there, so the next
    /// era starts with `height` as its consensus genesis. Run between eras on
    /// a stopped validator (a live stack would race the mutation). The mock's
    /// pre-consensus history is opaque — kept blocks simply become part of it,
    /// exactly like [`Self::anchored`] treats a migrated chain's past.
    pub fn fork_to(&self, height: u64) {
        let mut inner = self.inner.lock().unwrap();
        let tip = inner.anchor_height + inner.committed.len() as u64;
        assert!(
            height >= inner.anchor_height && height <= tip,
            "cannot fork to {height}: this chain covers {}..={tip}",
            inner.anchor_height
        );
        inner.anchor_height = height;
        inner.committed.clear();
    }

    /// A chain with `anchor_height` blocks of pre-consensus history; consensus starts
    /// at `anchor_height + 1` on top of the anchored genesis.
    pub fn anchored(anchor_height: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                anchor_height,
                committed: Vec::new(),
                idle: None,
            })),
        }
    }

    /// A chain restored from durable state — the mock equivalent of a node whose
    /// write-ahead log survived a restart alongside its consensus storage (the
    /// replay gate rebuilds environments this way from its fixture).
    pub fn with_committed_chain(committed: Vec<SimBlock>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                anchor_height: 0,
                committed,
                idle: None,
            })),
        }
    }

    /// Opts this environment into the cluster's shared idle-work pool; attach
    /// the same [`IdleWork`] to every validator's environment.
    pub fn attach_idle(&self, work: IdleWork) {
        self.inner.lock().unwrap().idle = Some(work);
    }

    /// The committed chain so far (test probe).
    pub fn committed_chain(&self) -> Vec<SimBlock> {
        self.inner.lock().unwrap().committed.clone()
    }

    /// Height of the last committed block — the anchor height counts as committed
    /// (pre-consensus history is durable by definition). `None` only for a fresh
    /// unanchored chain (test probe).
    pub fn committed_tip(&self) -> Option<u64> {
        let inner = self.inner.lock().unwrap();
        let tip = inner.anchor_height + inner.committed.len() as u64;
        (tip > 0).then_some(tip)
    }
}

impl SimEnv for MockExecution {
    fn era_anchor(&self) -> u64 {
        self.inner.lock().unwrap().anchor_height
    }

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
        let anchor_height = self.inner.lock().unwrap().anchor_height;
        if anchor_height == 0 {
            SimBlock::genesis()
        } else {
            SimBlock::anchor(anchor_height)
        }
    }

    async fn build(&mut self, parent: SimBlock, context: BuildContext) -> Option<SimBlock> {
        {
            let inner = self.inner.lock().unwrap();
            if let Some(work) = &inner.idle {
                let mut shared = work.0.lock().unwrap();
                let now = (shared.now)();
                let child = parent.height_u64() + 1;
                if shared.work_heights.contains(&child) {
                    // An abandoned work block being rebuilt: same work, no new unit.
                } else if shared.pending_work > 0 {
                    shared.pending_work -= 1;
                    shared.work_heights.insert(child);
                } else {
                    use zksync_os_consensus_core::idle_policy::IdleDecision;
                    let parent_number = parent.height_u64();
                    let parent_time = shared
                        .built_at
                        .get(&parent_number)
                        .copied()
                        .unwrap_or(shared.last_progress);
                    match shared.policy.decide(parent.era_height(), parent_time, now) {
                        IdleDecision::Decline => return None,
                        IdleDecision::BuildEmpty(_) => {}
                    }
                }
                // This turn builds: stamp the child the way a real block's
                // header would carry its timestamp.
                shared.built_at.insert(parent.height_u64() + 1, now);
            }
        }
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
        // Consensus counts heights from the era anchor; the mock's ledger counts
        // the chain. Translate at the boundary.
        let anchor = self.inner.lock().unwrap().anchor_height;
        self.committed_tip()
            .map(|tip| Height::new(tip.saturating_sub(anchor)))
    }

    async fn commit(&mut self, block: SimBlock) {
        let mut inner = self.inner.lock().unwrap();
        let height = block.height_u64();
        assert!(
            height > inner.anchor_height,
            "consensus committed height {height} at or below the anchor {}",
            inner.anchor_height,
        );
        let next_height = inner.anchor_height + inner.committed.len() as u64 + 1;
        if height < next_height {
            // At-least-once delivery: after a restart, consensus replays blocks the node
            // already has. They must be the *same* blocks — a mismatch here would mean
            // two conflicting blocks were finalized at one height, the one thing BFT
            // consensus exists to prevent.
            let existing = &inner.committed[(height - inner.anchor_height - 1) as usize];
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
        if let Some(work) = &inner.idle {
            let mut shared = work.0.lock().unwrap();
            shared.last_progress = (shared.now)();
        }
    }
}

/// Wraps an execution environment, delaying chosen operations by virtual time — the
/// deterministic stand-in for a validator whose execution is slow (long `verify`) or
/// whose persistence lags (long `commit`). Everything else delegates unchanged.
#[derive(Clone)]
pub struct DelayedEnv<X> {
    inner: X,
    context: std::sync::Arc<deterministic::Context>,
    verify_delay: Duration,
    commit_delay: Duration,
}

impl<X: SimEnv> DelayedEnv<X> {
    pub fn slow_verify(inner: X, context: deterministic::Context, delay: Duration) -> Self {
        let context = std::sync::Arc::new(context);
        Self {
            inner,
            context,
            verify_delay: delay,
            commit_delay: Duration::ZERO,
        }
    }

    pub fn slow_commit(inner: X, context: deterministic::Context, delay: Duration) -> Self {
        let context = std::sync::Arc::new(context);
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
    fn era_anchor(&self) -> u64 {
        self.inner.era_anchor()
    }

    fn committed_tip(&self) -> Option<u64> {
        self.inner.committed_tip()
    }

    fn committed_chain_digests(&self) -> Vec<<Self::Block as Digestible>::Digest> {
        self.inner.committed_chain_digests()
    }
}
