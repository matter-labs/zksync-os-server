//! Delivers finalized blocks from consensus into the execution environment.
//!
//! Consensus (marshal) reports finalized blocks strictly in height order and waits for
//! acknowledgements before letting more than a bounded window of blocks pile up — the
//! execution side is the pacer. Delivery is at-least-once: after a restart, blocks the
//! node already committed can be delivered again, which [`ExecutionEnv::commit`] is
//! required to absorb as a no-op.
//!
//! `Reporter::report` is synchronous (it returns a backpressure verdict, not a future),
//! so the committer is a pair: the reporter half enqueues `(block, ack)` and returns
//! immediately, and a worker task owned by the stack commits and acknowledges in order.
//! The queue is unbounded on purpose — marshal's own pending-ack window is the real
//! bound, and it only ever holds a handful of blocks.

use crate::execution::ExecutionEnv;
use commonware_actor::Feedback;
use commonware_consensus::marshal::Update;
use commonware_consensus::{Heightable as _, Reporter};
use commonware_runtime::{Handle, Spawner};
use commonware_utils::acknowledgement::{Acknowledgement, Exact};
use futures::StreamExt as _;
use futures::channel::mpsc;
use tracing::{debug, info};

/// Applies each finalized block via [`ExecutionEnv::commit`] and acknowledges it only
/// after the commit is durable. Plugged into consensus as the finalized-block consumer.
#[derive(Clone)]
pub struct FinalizedBlockCommitter<X: ExecutionEnv> {
    deliveries: mpsc::UnboundedSender<(X::Block, Exact)>,
}

impl<X: ExecutionEnv> FinalizedBlockCommitter<X> {
    /// Spawns the commit worker on `context` and returns the reporter half plus the
    /// worker's handle (the stack aborts it on shutdown like any other component).
    pub fn spawn<R: Spawner>(context: R, mut env: X) -> (Self, Handle<()>) {
        let (deliveries, mut queue) = mpsc::unbounded::<(X::Block, Exact)>();
        let worker = context.spawn(move |_context| async move {
            while let Some((block, ack)) = queue.next().await {
                let height = block.height();
                // Consensus height zero is the era anchor: the genesis block (or the
                // migration cutover block) whose state every validator already holds.
                // Marshal delivers it once at startup for completeness; there is
                // nothing to apply.
                if height.is_zero() {
                    ack.acknowledge();
                    continue;
                }
                env.commit(block).await;
                // Acknowledging tells consensus the block is durable and it may deliver
                // the next one. Acknowledge strictly after commit: acking early would let
                // a crash lose a block that consensus considers delivered.
                ack.acknowledge();
                info!(%height, "committed finalized block");
            }
        });
        (Self { deliveries }, worker)
    }
}

impl<X: ExecutionEnv> Reporter for FinalizedBlockCommitter<X> {
    type Activity = Update<X::Block, Exact>;

    fn report(&mut self, update: Self::Activity) -> Feedback {
        match update {
            Update::Tip(round, height, _digest) => {
                // The network finalized up to `height`; the blocks themselves may still be
                // on their way (backfill). Nothing to do — commits happen on Block updates.
                debug!(?round, %height, "observed finalized tip");
                Feedback::Ok
            }
            Update::Block(block, ack) => {
                if self.deliveries.unbounded_send((block, ack)).is_err() {
                    // The worker is gone: the stack is shutting down. Marshal treats a
                    // dropped acknowledgement as exactly that signal.
                    return Feedback::Closed;
                }
                Feedback::Ok
            }
        }
    }
}
