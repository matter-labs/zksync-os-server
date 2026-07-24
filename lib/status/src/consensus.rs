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
    /// Size of the committee holding `epoch` (committees may change per epoch).
    pub committee_size: u32,
    pub observed_unix: u64,
}

/// Encodes the consensus runtime's own prometheus registry (engine, marshal, p2p
/// actors) on demand. Installed by the consensus thread once its runtime is up.
pub type ConsensusMetricsEncoder = Arc<dyn Fn() -> String + Send + Sync>;

/// The latest on-chain-registry derivation this node performed (shadow and
/// config-shadow modes). External monitors judge registry health from it two
/// ways: `matches_config` / `outcome` on one node (drift or refusal against
/// this node's own config), and `committee_hash` compared *across* nodes for
/// the same `last_epoch` (two nodes deriving different committees from the
/// same chain state would split a registry-governed committee — both modes
/// exist to surface that while config still governs or mirrors).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryStatus {
    /// `"shadow"` or `"config_shadow"` (the `schedule` mode serves no section).
    pub mode: String,
    /// The newest epoch whose derivation completed.
    pub last_epoch: u64,
    /// The chain-absolute height its registry state was read at.
    pub last_lookahead_height: u64,
    /// `"derived"`, `"carried_no_entry"`, or `"carried_refused"`.
    pub outcome: String,
    /// Whether the derived committee equals the config schedule's answer.
    pub matches_config: bool,
    /// Human-readable refusal reason, present on `carried_refused`.
    pub refusal: Option<String>,
    /// Canonical hash of the committee in effect (first 8 bytes of sha256 over
    /// the ordered member keys, hex) — compare across validators.
    pub committee_hash: String,
    pub committee_size: usize,
}

/// Where the status server reads a pending scheduled cutover from. Present only
/// while consensus is configured to start at a chain height this node has not
/// reached yet: the node runs its pre-cutover role (sequencing or following)
/// until `genesis_height`, then restarts into consensus.
#[derive(Clone)]
pub struct ScheduledCutoverStatusSource {
    pub genesis_height: u64,
    /// The node's current write-ahead-log tip, updated by the cutover sentinel.
    pub tip: watch::Receiver<u64>,
}

impl ScheduledCutoverStatusSource {
    pub fn snapshot(&self) -> ScheduledCutoverStatus {
        ScheduledCutoverStatus {
            genesis_height: self.genesis_height,
            tip: *self.tip.borrow(),
        }
    }
}

/// The `scheduled_cutover` section of a `/status` response. `tip` advancing
/// toward `genesis_height` is the migration's progress indicator; the section
/// disappears once the node restarts into consensus (replaced by `consensus`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScheduledCutoverStatus {
    pub genesis_height: u64,
    pub tip: u64,
}

/// Where the status server reads consensus facts from.
pub struct ConsensusStatusSource {
    /// `"validator"` or `"observer"` — whether this node votes or only follows.
    pub role: &'static str,
    pub committee_size: usize,
    /// This validator's network identity (ed25519 public key, hex).
    pub validator: String,
    pub finalized: watch::Receiver<Option<FinalizedObservation>>,
    /// Height of the last block durably applied by this node's pipeline.
    pub applied_height: watch::Receiver<Option<u64>>,
    /// Highest height *covered* by the node's own finality store: certified at
    /// that height or by a later stored certificate over the contiguous digest
    /// trail (see the consensus execution crate's finality store). Tracks the
    /// tip on a healthy chain; a stall is a real health signal.
    pub finality_certified: watch::Receiver<Option<u64>>,
    /// Canonical hash of the committee-uniform configuration surface (schedule,
    /// chain constants, consensus timing). Identical on every healthy committee
    /// member; a mismatch is config drift caught *before* it becomes a boundary
    /// stall or a false byzantine alarm.
    pub chain_fingerprint: String,
    /// The latest registry derivation; `None` until the first one (and forever
    /// in `schedule` mode).
    pub registry: watch::Receiver<Option<RegistryStatus>>,
    pub metrics_encoder: watch::Receiver<Option<ConsensusMetricsEncoder>>,
}

/// The consensus section of a `/status` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusStatus {
    /// `"validator"` or `"observer"`.
    #[serde(default)]
    pub role: String,
    pub committee_size: usize,
    pub validator: String,
    /// Consensus-side progress: the latest finalized round observed.
    pub finalized: Option<FinalizedObservation>,
    /// Node-side progress: the height this node has durably applied.
    pub applied_height: Option<u64>,
    /// Every height up to this one is covered by the node's stored finality
    /// certificates (its own height's, or a later one over the contiguous
    /// digest trail) — the externally-provable-finality trail.
    #[serde(default)]
    pub finality_certified_height: Option<u64>,
    /// Canonical hash of the committee-uniform configuration surface; compare
    /// across validators — any mismatch is config drift.
    #[serde(default)]
    pub chain_fingerprint: String,
    /// The latest on-chain-registry derivation (shadow/config_shadow modes only).
    #[serde(default)]
    pub registry: Option<RegistryStatus>,
}

impl ConsensusStatusSource {
    pub(crate) fn snapshot(&self) -> ConsensusStatus {
        ConsensusStatus {
            role: self.role.to_string(),
            committee_size: self.committee_size,
            validator: self.validator.clone(),
            finalized: *self.finalized.borrow(),
            applied_height: *self.applied_height.borrow(),
            finality_certified_height: *self.finality_certified.borrow(),
            chain_fingerprint: self.chain_fingerprint.clone(),
            registry: self.registry.borrow().clone(),
        }
    }
}
