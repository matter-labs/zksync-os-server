//! Delivers finalized blocks from consensus into the execution environment.
//!
//! Consensus (marshal) reports finalized blocks strictly in height order and waits for an
//! acknowledgement before letting more than a bounded window of blocks pile up — the
//! execution side is the pacer. Delivery is at-least-once: after a restart, blocks the
//! node already committed can be delivered again, which [`ExecutionEnv::commit`] is
//! required to absorb as a no-op.

use crate::execution::ExecutionEnv;
use commonware_consensus::Reporter;
use commonware_consensus::marshal::Update;
use commonware_utils::acknowledgement::{Acknowledgement, Exact};
use tracing::debug;

/// Applies each finalized block via [`ExecutionEnv::commit`] and acknowledges it only
/// after the commit is durable. Plugged into consensus as the finalized-block consumer.
#[derive(Clone)]
pub struct FinalizedBlockCommitter<X: ExecutionEnv> {
    env: X,
}

impl<X: ExecutionEnv> FinalizedBlockCommitter<X> {
    pub fn new(env: X) -> Self {
        Self { env }
    }
}

impl<X: ExecutionEnv> Reporter for FinalizedBlockCommitter<X> {
    type Activity = Update<X::Block, Exact>;

    async fn report(&mut self, update: Self::Activity) {
        match update {
            Update::Tip(round, height, _digest) => {
                // The network finalized up to `height`; the blocks themselves may still be
                // on their way (backfill). Nothing to do — commits happen on Block updates.
                debug!(?round, %height, "observed finalized tip");
            }
            Update::Block(block, ack) => {
                self.env.commit(block).await;
                // Acknowledging tells consensus the block is durable and it may deliver
                // the next one. Acknowledge strictly after commit: acking early would let
                // a crash lose a block that consensus considers delivered.
                ack.acknowledge();
            }
        }
    }
}
