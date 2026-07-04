//! The consensus section of `/status`, plus the consensus runtime's metrics route.
//!
//! Everything live comes from watch channels the node updates; nodes that do not run
//! consensus serve `consensus: null` and 404 on the metrics route. This surface is
//! what external monitors (staging dashboards, the chaos rig's invariant checker)
//! poll to judge a validator's health without parsing logs.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::watch;

/// A finalized round this validator observed, stamped when it was seen. The view
/// advancing is the consensus-side liveness signal; `observed_unix` going stale while
/// the committee is supposedly healthy is the "no finalization for X seconds" alert.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FinalizedObservation {
    pub epoch: u64,
    pub view: u64,
    pub observed_unix: u64,
}

/// Encodes the consensus runtime's own prometheus registry (engine, marshal, p2p
/// actors) on demand. Installed by the consensus thread once its runtime is up.
pub type ConsensusMetricsEncoder = Arc<dyn Fn() -> String + Send + Sync>;

/// Where the status server reads consensus facts from.
pub struct ConsensusStatusSource {
    pub committee_size: usize,
    /// This validator's network identity (ed25519 public key, hex).
    pub validator: String,
    pub finalized: watch::Receiver<Option<FinalizedObservation>>,
    /// Height of the last block durably applied by this node's pipeline.
    pub applied_height: watch::Receiver<Option<u64>>,
    pub metrics_encoder: watch::Receiver<Option<ConsensusMetricsEncoder>>,
}

/// The consensus section of a `/status` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusStatus {
    pub committee_size: usize,
    pub validator: String,
    /// Consensus-side progress: the latest finalized round observed.
    pub finalized: Option<FinalizedObservation>,
    /// Node-side progress: the height this node has durably applied.
    pub applied_height: Option<u64>,
}

impl ConsensusStatusSource {
    pub(crate) fn snapshot(&self) -> ConsensusStatus {
        ConsensusStatus {
            committee_size: self.committee_size,
            validator: self.validator.clone(),
            finalized: *self.finalized.borrow(),
            applied_height: *self.applied_height.borrow(),
        }
    }
}
