//! The boundary between consensus and block execution.
//!
//! Consensus decides *which* block is next; execution decides *what* a valid block is and
//! *applies* the decided ones. Everything consensus needs from the node goes through
//! [`ExecutionEnv`], and nothing else. This keeps the consensus stack runnable against a
//! mock (or an in-memory real-STF) implementation in deterministic simulation tests, with
//! the production implementation living next to the sequencer.

use commonware_consensus::Block;
use commonware_consensus::types::Height;
use std::future::Future;

/// Consensus-side information available when building a new block.
///
/// Deliberately minimal: the execution side owns everything content-related (transaction
/// selection, timestamps, fees). Consensus only says where in the protocol the block lands.
#[derive(Debug, Clone, Copy)]
pub struct BuildContext {
    /// The consensus epoch this block is proposed in.
    pub epoch: u64,
    /// The consensus view (round within the epoch) this block is proposed in.
    pub view: u64,
}

/// What consensus asks of the execution side. Implementations must be cheap to clone
/// (clones share the same underlying state, actor-handle style).
///
/// There are exactly four interactions:
///
/// - `genesis_block`: the agreed-upon first block everyone starts from.
/// - `build`: leader path — produce a fully-executed block on top of `parent`.
/// - `verify`: follower path — decide whether a proposed block is valid, *before* voting
///   for it. By the time this is called, both the block and its parent are locally
///   available; the expected implementation re-executes the block and compares outputs.
/// - `commit`: a block (and by construction its whole ancestry) is final — apply it
///   durably. Delivered strictly in height order, but **at least once**: after a restart
///   the same block may be delivered again, so implementations must treat re-commits of
///   an already-committed height as a no-op.
///
/// Cancellation: consensus abandons `build`/`verify` calls whose view has ended by
/// dropping the future. Implementations must tolerate that (no partial global state).
pub trait ExecutionEnv: Clone + Send + 'static {
    /// The block type this execution environment produces and applies.
    type Block: Block + Clone;

    /// The first block of the chain. Every validator must derive the identical block —
    /// its digest is the root of the whole chain and part of the network's configuration.
    fn genesis_block(&mut self) -> impl Future<Output = Self::Block> + Send;

    /// Build a fully-executed block on top of `parent`. Returning `None` means "nothing
    /// to propose" (e.g. the builder failed); consensus will let the view time out and
    /// move to the next leader — a routine event, not an error.
    fn build(
        &mut self,
        parent: Self::Block,
        context: BuildContext,
    ) -> impl Future<Output = Option<Self::Block>> + Send;

    /// Fully validate `block` against its (already-validated) `parent`. Returns whether
    /// this validator vouches for the block. The verdict is scoped to the current round:
    /// `false` withholds this validator's vote (the view times out and rotates), and a
    /// later re-proposal is verified afresh — so "cannot validate against my current
    /// knowledge yet" safely answers `false` too.
    fn verify(
        &mut self,
        parent: Self::Block,
        block: Self::Block,
    ) -> impl Future<Output = bool> + Send;

    /// Whether this environment can already execute children of `block` — i.e. it holds
    /// the block's resulting state (committed, or speculatively from an earlier
    /// build/verify). When a proposal's parent has no state yet (typically: a restart
    /// discarded speculative state that peers still build on), the caller walks the
    /// ancestry down to a block that has, and verifies forward from there. The default
    /// says "always" — for environments that never lose speculative state.
    fn has_state(&mut self, _block: &Self::Block) -> impl Future<Output = bool> + Send {
        async { true }
    }

    /// The highest block height this environment has durably committed, or `None` for a
    /// fresh chain. Used on startup so consensus does not re-deliver history the node
    /// already has.
    fn committed_height(&mut self) -> impl Future<Output = Option<Height>> + Send;

    /// Startup hand-back of the finalized block at the environment's committed height,
    /// read from the consensus archive. Lets an environment that persists chain content
    /// but not consensus identities (digests) re-anchor its tip after a restart. The
    /// default does nothing.
    fn adopt_committed_block(&mut self, _block: &Self::Block) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Durably apply a finalized block. Must be idempotent (at-least-once delivery) and
    /// must only return once the block would survive a crash — consensus acknowledges
    /// delivery (and allows the next block through) when this returns.
    fn commit(&mut self, block: Self::Block) -> impl Future<Output = ()> + Send;
}
