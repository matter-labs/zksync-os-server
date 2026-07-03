//! Adapts an [`ExecutionEnv`] to the application interface consensus expects.
//!
//! The consensus machinery (the simplex engine plus marshal's `Inline` wrapper) handles
//! everything protocol-side: fetching blocks and parents by digest, backfilling gaps from
//! peers, structural ancestry checks (parent digest linkage, height contiguity), and
//! epoch-boundary rules. What remains — and what this adapter forwards to the execution
//! environment — is the application-semantics part: building a block's content and judging
//! a block's validity given its parent.

use crate::execution::{BuildContext, ExecutionEnv};
use crate::types::Scheme;
use commonware_consensus::marshal::ancestry::{AncestorStream, BlockProvider};
use commonware_consensus::simplex::types::Context;
use commonware_consensus::{Application, VerifyingApplication};
use commonware_cryptography::Digestible;
use commonware_cryptography::ed25519::PublicKey;
use commonware_runtime::{Clock, Metrics, Spawner};
use futures::StreamExt;
use rand08::Rng;
use tracing::warn;

/// The consensus application: a thin, cloneable adapter over an [`ExecutionEnv`].
#[derive(Clone)]
pub struct ExecutionApplication<X: ExecutionEnv> {
    env: X,
}

impl<X: ExecutionEnv> ExecutionApplication<X> {
    pub fn new(env: X) -> Self {
        Self { env }
    }
}

type Digest<X> = <<X as ExecutionEnv>::Block as Digestible>::Digest;

impl<R, X> Application<R> for ExecutionApplication<X>
where
    R: Rng + Spawner + Metrics + Clock,
    X: ExecutionEnv,
{
    type SigningScheme = Scheme;
    type Context = Context<Digest<X>, PublicKey>;
    type Block = X::Block;

    async fn genesis(&mut self) -> Self::Block {
        self.env.genesis_block().await
    }

    /// Leader path. The ancestry stream yields the parent first (deeper ancestors on
    /// further pulls, fetched on demand — we only need the parent).
    async fn propose<P: BlockProvider<Block = Self::Block>>(
        &mut self,
        (_runtime, context): (R, Self::Context),
        mut ancestry: AncestorStream<P, Self::Block>,
    ) -> Option<Self::Block> {
        let Some(parent) = ancestry.next().await else {
            // The parent became unavailable (e.g. the view ended and consensus tore the
            // stream down). Not proposing is always safe.
            warn!("parent unavailable while proposing; skipping proposal");
            return None;
        };
        let build_context = BuildContext {
            epoch: context.round.epoch().get(),
            view: context.round.view().get(),
        };
        self.env.build(parent, build_context).await
    }
}

impl<R, X> VerifyingApplication<R> for ExecutionApplication<X>
where
    R: Rng + Spawner + Metrics + Clock,
    X: ExecutionEnv,
{
    /// Follower path, called before this validator votes for the block. The ancestry
    /// stream yields the block under verification first, then its parent. Structural
    /// linkage (parent digest, height contiguity) is already checked by the caller;
    /// the execution environment judges content validity.
    async fn verify<P: BlockProvider<Block = Self::Block>>(
        &mut self,
        (_runtime, _context): (R, Self::Context),
        mut ancestry: AncestorStream<P, Self::Block>,
    ) -> bool {
        let (Some(block), Some(parent)) = (ancestry.next().await, ancestry.next().await) else {
            // The caller seeds the stream with the block and its parent already fetched,
            // so these two pulls cannot come up empty today. If that ever changes, `false`
            // here only withholds this validator's vote for this round — it does not stop
            // the network from finalizing the block, and this node would then still apply
            // it through the finalization path.
            warn!("ancestry unavailable while verifying; withholding vote");
            return false;
        };
        self.env.verify(parent, block).await
    }
}
