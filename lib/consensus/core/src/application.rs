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
use commonware_consensus::Application;
use commonware_consensus::marshal::ancestry::Ancestry;
use commonware_consensus::simplex::types::Context;
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

impl<E, X> Application<E> for ExecutionApplication<X>
where
    E: Rng + Spawner + Metrics + Clock,
    X: ExecutionEnv,
{
    type SigningScheme = Scheme;
    type Context = Context<Digest<X>, PublicKey>;
    type Block = X::Block;

    /// Leader path. The ancestry yields the parent first (deeper ancestors on
    /// further pulls, fetched on demand — we only need the parent).
    async fn propose(
        &mut self,
        (_runtime, context): (E, Self::Context),
        mut ancestry: impl Ancestry<Self::Block>,
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

    /// Follower path, called before this validator votes for the block. The ancestry
    /// yields the block under verification first, then its parent, then deeper
    /// ancestors on demand. Structural linkage (parent digest, height contiguity) is
    /// already checked by the caller; the execution environment judges content
    /// validity.
    ///
    /// Verification may need more than the direct parent: after a restart this
    /// validator has its durable chain but none of the speculative state for blocks
    /// its peers verified while it was away (e.g. a notarized-but-unfinalized block
    /// everyone now builds on). Walk the ancestry down to the first block whose state
    /// this environment holds, then verify forward — each step re-executes one
    /// ancestor and rebuilds its speculative state.
    ///
    /// Returning `false` is reserved for permanent invalidity; upstream treats an
    /// unresolved future as abstention. Our negative verdicts are all "invalid for
    /// this round as observed" and a fresh proposal is judged fresh, so resolving
    /// `false` (rather than hanging the vote) remains the right mapping.
    async fn verify(
        &mut self,
        (_runtime, _context): (E, Self::Context),
        mut ancestry: impl Ancestry<Self::Block>,
    ) -> bool {
        // Deeper than any healthy unfinalized window; a walk this long means state is
        // unrecoverable through ancestry and the vote should be withheld.
        const MAX_WALK: usize = 256;

        let Some(block) = ancestry.next().await else {
            warn!("block unavailable while verifying; withholding vote");
            return false;
        };
        let mut chain = vec![block];
        loop {
            let Some(ancestor) = ancestry.next().await else {
                // The stream ends when the view is torn down or history is missing.
                // Withholding is safe: this only skips this validator's vote for this
                // round, and a finalized block still arrives via the commit path.
                warn!("ancestry unavailable while verifying; withholding vote");
                return false;
            };
            let anchored = self.env.has_state(&ancestor).await;
            chain.push(ancestor);
            if anchored {
                break;
            }
            if chain.len() > MAX_WALK {
                warn!(
                    depth = chain.len(),
                    "no known state within the verification walk; withholding vote"
                );
                return false;
            }
        }

        // `chain` is [block, parent, ..., anchor] — verify from the anchor upward.
        while chain.len() >= 2 {
            let parent = chain.pop().expect("checked length");
            let child = chain.last().cloned().expect("checked length");
            if !self.env.verify(parent, child).await {
                return false;
            }
        }
        true
    }
}
