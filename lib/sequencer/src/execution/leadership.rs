use futures::future;
use tokio::sync::watch;

/// Whether this node is currently allowed to produce new blocks.
///
/// Only the leader produces blocks; a replica only re-executes blocks that were
/// produced elsewhere and delivered through the canonization fence
/// (see [`BlockCanonization`](crate::execution::BlockCanonization)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsensusRole {
    Leader,
    Replica,
}

/// Tells the block production loop whether this node is the leader right now.
///
/// This is the second half of the consensus seam (next to [`BlockCanonization`]):
/// consensus decides *who* produces blocks (this signal) and *which* produced
/// blocks become canonical (the canonization fence).
///
/// [`BlockCanonization`]: crate::execution::BlockCanonization
#[derive(Debug, Clone)]
pub enum LeadershipSignal {
    /// Single-sequencer mode: no consensus, this node is always the leader.
    AlwaysLeader,
    /// Consensus mode: the current role is driven by the consensus engine.
    Watch(watch::Receiver<ConsensusRole>),
}

impl LeadershipSignal {
    pub fn current_role(&self) -> ConsensusRole {
        match self {
            Self::AlwaysLeader => ConsensusRole::Leader,
            Self::Watch(rx) => *rx.borrow(),
        }
    }

    /// Completes when the role may have changed; never completes in
    /// single-sequencer mode.
    pub async fn wait_for_change(&mut self) -> Result<(), watch::error::RecvError> {
        match self {
            Self::AlwaysLeader => future::pending::<Result<(), watch::error::RecvError>>().await,
            Self::Watch(rx) => rx.changed().await,
        }
    }
}
