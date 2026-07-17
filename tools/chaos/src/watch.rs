//! The watcher: continuously checks that the cluster upholds the consensus
//! algorithm's properties, cross-referenced against what the driver injected.
//!
//! Two classes of check. The **precise** ones need no timing judgment and hold at
//! every instant:
//!
//! - *agreement*: every reachable validator serves the identical block hash at the
//!   highest height they all have;
//! - *finality is monotone*: no validator's finalized `(epoch, view)` or applied
//!   height ever goes backwards (views restart at epoch boundaries — only the pair
//!   is monotone);
//! - *no progress without quorum*: while the driver holds the healthy set below
//!   quorum, no new finalization may appear (beyond a settle margin for certificates
//!   already in flight when quorum was lost) — progress there would mean finality
//!   without enough validators, the worst possible outcome;
//! - *no protocol fault evidence*: on this honest committee, the consensus evidence
//!   counters (conflicting votes) must stay at zero;
//! - *no unexpected deaths*: a container the driver did not touch must be running;
//! - *clean logs*: no panics or ERROR lines beyond the known-benign shutdown noise of
//!   nodes the driver itself stopped.
//!
//! The **windowed** ones judge liveness and are deliberately generous: when the
//! driver expects the committee to be live for a whole window, the finalized view
//! must advance within it. False positives are cheap on a rig — a human looks,
//! widens the window.
//!
//! On the first confirmed finding the watcher freezes the experiment: the driver
//! stops injecting, nothing is healed, artifacts (docker logs, journal, findings)
//! are captured, and the process exits nonzero — the scene stays up for
//! investigation.

use crate::drive::Condition;
use crate::setup::Manifest;
use serde::Serialize;
use std::io::Write as _;
use std::time::{Duration, Instant};
use zksync_os_status_server::StatusResponse;

/// What the driver currently believes about the cluster — the watcher's ground truth
/// for "is a stall a finding or self-inflicted".
#[derive(Debug, Clone)]
pub struct Expectations {
    pub conditions: Vec<Condition>,
    pub expect_liveness: bool,
    /// When `expect_liveness` last changed.
    pub since: Instant,
    /// The driver is holding the L1 dark. Consensus liveness stays expected —
    /// that is the point of the fault — but L1-facing components legitimately
    /// scream about connectivity, so those specific log lines are tolerated
    /// while this holds (and briefly after, for lines still flushing).
    pub l1_blackout: bool,
}

/// One validator, as observed during a poll.
#[derive(Debug, Clone, Serialize)]
pub struct NodeObservation {
    /// `None` when the docker probe itself failed (a busy daemon times out under
    /// restart churn) — state unknown, not dead; a real death is confirmed by the
    /// next successful poll.
    pub running: Option<bool>,
    pub paused: bool,
    /// The container's `.State.StartedAt` — a restart proof that does not depend
    /// on the polls happening to observe the Killed/Stopped window. Under storm
    /// cadence a kill→start pair can fall entirely between two polls (cycles
    /// stretch to ~12s while heal horizons run as short as 9s), and the
    /// condition-transition reset below would miss it.
    pub started_at: Option<String>,
    /// `None` when unreachable (which is fine for stopped/paused/partitioned nodes).
    /// `(epoch, view)`: views restart from zero at every epoch boundary, so only the
    /// pair is monotone — comparing bare views across a boundary reads a healthy
    /// handoff as a finality regression.
    pub finalized_round: Option<(u64, u64)>,
    pub applied_height: Option<u64>,
    pub block_hash_at_probe: Option<String>,
    /// What this node says the probed block *did*: its transaction list and a
    /// sample of receipts, condensed for cross-node comparison.
    pub execution_at_probe: Option<ExecutionFingerprint>,
    /// Sum of the consensus fault-evidence counters.
    pub evidence: u64,
    /// The node's `consensus_verify_verdicts{verdict="invalid"}` counter: how
    /// many peer proposals it has permanently rejected (linkage, validity, or
    /// re-execution mismatch). Never ticks on an honest, converged cluster.
    pub verify_invalid: u64,
    /// The committee-uniform configuration fingerprint the node serves; any
    /// two healthy members must agree.
    pub chain_fingerprint: Option<String>,
    /// The node's latest on-chain-registry derivation (`/status.consensus.registry`;
    /// absent on nodes not running the registry in shadow or config-shadow mode).
    pub registry: Option<zksync_os_status_server::RegistryStatus>,
    /// Log lines since the previous poll that match the forbidden patterns.
    pub suspicious_log_lines: Vec<String>,
}

/// A block's execution outputs as served over RPC, condensed to strings that
/// either match across validators or convict someone. The receipt sample is
/// deterministic (the block's first transactions), so every node summarizes
/// the same subset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionFingerprint {
    /// The block's transaction hashes, joined — divergence here means nodes
    /// disagree on the block's *contents*.
    pub txs: String,
    /// `(tx hash, "status|gasUsed|logsBloom")` for the sampled transactions —
    /// divergence here means nodes executed the same block differently (or
    /// serve corrupted results).
    pub receipts: Vec<(String, String)>,
}

/// The height whose hash every reachable validator was asked about this poll.
#[derive(Debug, Clone, Serialize)]
pub struct Poll {
    pub probe_height: Option<u64>,
    pub nodes: Vec<NodeObservation>,
    /// Settlement progress read from L1 itself (the diamond's batch counters) —
    /// deliberately not from any node, so the reading survives the settler.
    /// `None` when the L1 probe failed (blackouts, busy anvil).
    pub settlement: Option<SettlementObservation>,
}

/// The chain's settlement counters as L1 reports them.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SettlementObservation {
    pub committed_batches: u64,
    pub executed_batches: u64,
}

#[derive(Debug, Clone, Serialize)]
pub enum Finding {
    /// Two validators serve different blocks at the same height — an agreement
    /// violation, the property the whole protocol exists to provide.
    HashDisagreement {
        height: u64,
        hashes: Vec<(usize, String)>,
    },
    /// A validator's applied height went backwards.
    FinalityRegression {
        validator: usize,
        what: &'static str,
        from: u64,
        to: u64,
    },
    /// A validator's finalized `(epoch, view)` went backwards. Tracked as the pair
    /// because views restart at epoch boundaries.
    RoundRegression {
        validator: usize,
        from: (u64, u64),
        to: (u64, u64),
    },
    /// A new finalization appeared while the driver held the committee below quorum.
    ProgressWithoutQuorum {
        baseline_round: (u64, u64),
        observed_round: (u64, u64),
    },
    /// A consensus fault-evidence counter ticked on an honest committee.
    FaultEvidence { validator: usize, count: u64 },
    /// A container the driver did not touch is not running.
    UnexpectedDeath { validator: usize },
    /// A forbidden log line (panic / unexplained ERROR).
    SuspiciousLog { validator: usize, line: String },
    /// Validators disagree about a finalized block's execution: its transaction
    /// list or a sampled receipt differs between nodes that agree on the hash
    /// chain — an RPC/storage-layer divergence.
    ExecutionDisagreement {
        height: u64,
        what: &'static str,
        details: Vec<(usize, String)>,
    },
    /// A validator permanently rejected a peer proposal (`verify_verdicts`
    /// `invalid` ticked). On an honest cluster this is the STF-divergence
    /// tripwire: some peer built a block this node's re-execution refuses.
    VerifyRejection { validator: usize, count: u64 },
    /// Validators serve different committee-uniform config fingerprints —
    /// config drift, caught before an epoch boundary or upgrade makes it
    /// expensive.
    ConfigDrift { fingerprints: Vec<(usize, String)> },
    /// The on-chain registry's derivation went wrong somewhere: a node derived
    /// a committee its config schedule disagrees with (in shadow mode the
    /// registry is out of sync, in config-shadow mode the config mirror is),
    /// a derivation failed validation (rotation via the registry is blocked),
    /// or two nodes derived different committees for the same epoch — the
    /// failure that would split a registry-governed committee, caught here
    /// while something else still governs or mirrors.
    RegistryDrift {
        what: &'static str,
        details: Vec<(usize, String)>,
    },
    /// The chain kept finalizing blocks but L1 settlement did not advance for the
    /// whole window, with the settler believed healthy — a dead-but-running
    /// settler, a wedged sender, or a prover pipeline stall. (A *down* settler
    /// is sanctioned fault territory; this fires only when nobody is touching it.)
    SettlementStall {
        counter: &'static str,
        stuck_at: u64,
        window: Duration,
    },
    /// The settlement probe persistently reads `executed > committed`, which the
    /// chain cannot produce — the probe (selectors, address, parsing) is
    /// miswired. A rig self-check, not a node property.
    SettlementProbeInsanity { committed: u64, executed: u64 },
    /// The committee was expected live for the whole window but no new finalization
    /// appeared.
    LivenessStall {
        window: Duration,
        at_round: (u64, u64),
        /// Who to look at first: validators whose own finalized round sits
        /// below the stalled tip (crashed? rejecting? isolated?).
        laggards: Vec<usize>,
    },
}

/// The pure check engine: state carried between polls, no I/O. The shell feeds it
/// observations; it returns findings.
pub struct Checker {
    /// Highest finalized `(epoch, view)` / applied height ever seen per validator.
    finalized_rounds: Vec<(u64, u64)>,
    applied_heights: Vec<u64>,
    evidence: Vec<u64>,
    verify_invalid: Vec<u64>,
    /// The driver's beliefs at the previous poll, for spotting heal transitions.
    previous_conditions: Vec<Condition>,
    /// Last observed container start times; a change is restart proof (see
    /// [`NodeObservation::started_at`]).
    started_at: Vec<Option<String>>,
    /// Highest finalized `(epoch, view)` seen anywhere, and when it last advanced.
    tip_round: (u64, u64),
    tip_advanced_at: Instant,
    /// The finalized-round baseline captured when liveness expectation was lost, once
    /// the settle margin passed. `None` while liveness is expected.
    forbidden_baseline: Option<(u64, u64)>,
    settle_margin: Duration,
    liveness_window: Duration,
    /// Which validator runs the batcher — settlement lag is only that node's
    /// fault while the driver believes it healthy.
    settler: usize,
    settlement_lag_window: Duration,
    /// Highest (committed, executed) batch counts seen on L1, and when each
    /// last advanced (or was last excused by a sanctioned condition).
    settlement_counters: (u64, u64),
    /// Consecutive polls with `executed > committed` — on-chain that invariant
    /// cannot break (executes require commits), so a *persistent* violation
    /// means the probe itself is miswired (the selector swap shipped with this
    /// oracle went unnoticed for want of exactly this check). One or two polls
    /// of excess are tolerated: the two counters are read in separate
    /// non-atomic eth_calls and can legitimately race each other.
    counter_insanity_streak: u32,
    committed_advanced_at: Instant,
    executed_advanced_at: Instant,
}

impl Checker {
    pub fn new(
        validators: usize,
        settle_margin: Duration,
        liveness_window: Duration,
        settler: usize,
        settlement_lag_window: Duration,
    ) -> Self {
        Self {
            finalized_rounds: vec![(0, 0); validators],
            applied_heights: vec![0; validators],
            evidence: vec![0; validators],
            verify_invalid: vec![0; validators],
            previous_conditions: vec![Condition::Healthy; validators],
            started_at: vec![None; validators],
            tip_round: (0, 0),
            tip_advanced_at: Instant::now(),
            forbidden_baseline: None,
            settle_margin,
            liveness_window,
            settler,
            settlement_lag_window,
            settlement_counters: (0, 0),
            counter_insanity_streak: 0,
            committed_advanced_at: Instant::now(),
            executed_advanced_at: Instant::now(),
        }
    }

    pub fn observe(
        &mut self,
        now: Instant,
        expectations: &Expectations,
        poll: &Poll,
    ) -> Vec<Finding> {
        let mut findings = Vec::new();

        // A node healed from Killed/Stopped has *restarted*: its applied height
        // is the applier's per-run progress and legitimately starts over from
        // the restart replay range. Its *reported* finalized round dips too:
        // with epoch retention, journal replay begins at the oldest retained
        // epoch, and /status reports that older round until replay catches up
        // (seen live: a restart at epoch 62 re-reporting epoch 60). Both
        // baselines reset; regressions on nodes that did not restart remain
        // findings. Unpause and reconnect do not restart the process — neither
        // resets for those.
        for (index, condition) in expectations.conditions.iter().enumerate() {
            if *condition == Condition::Healthy
                && matches!(
                    self.previous_conditions[index],
                    Condition::Killed | Condition::Stopped
                )
            {
                self.applied_heights[index] = 0;
                self.finalized_rounds[index] = (0, 0);
            }
        }
        self.previous_conditions = expectations.conditions.clone();

        // Restart proof independent of observed conditions: a changed container
        // start time means the process restarted (and is replaying), whatever
        // the polls happened to catch of the driver's kill→start window.
        for (index, node) in poll.nodes.iter().enumerate() {
            let Some(started) = &node.started_at else {
                continue;
            };
            if let Some(previous) = &self.started_at[index]
                && previous != started
            {
                self.applied_heights[index] = 0;
                self.finalized_rounds[index] = (0, 0);
            }
            self.started_at[index] = Some(started.clone());
        }

        // Agreement: everyone who answered the probe must agree.
        if let Some(height) = poll.probe_height {
            let hashes: Vec<(usize, String)> = poll
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(index, node)| {
                    node.block_hash_at_probe.clone().map(|hash| (index, hash))
                })
                .collect();
            if hashes.windows(2).any(|pair| pair[0].1 != pair[1].1) {
                findings.push(Finding::HashDisagreement { height, hashes });
            }

            // Execution agreement: matching hashes are necessary, not
            // sufficient — the nodes must also *serve* the same transaction
            // list and the same receipts for it.
            let prints: Vec<(usize, &ExecutionFingerprint)> = poll
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(index, node)| {
                    node.execution_at_probe.as_ref().map(|print| (index, print))
                })
                .collect();
            if prints.windows(2).any(|pair| pair[0].1.txs != pair[1].1.txs) {
                findings.push(Finding::ExecutionDisagreement {
                    height,
                    what: "transaction list",
                    details: prints
                        .iter()
                        .map(|(index, print)| (*index, print.txs.clone()))
                        .collect(),
                });
            } else if prints
                .windows(2)
                .any(|pair| pair[0].1.receipts != pair[1].1.receipts)
            {
                findings.push(Finding::ExecutionDisagreement {
                    height,
                    what: "sampled receipts",
                    details: prints
                        .iter()
                        .map(|(index, print)| (*index, format!("{:?}", print.receipts)))
                        .collect(),
                });
            }
        }

        for (index, node) in poll.nodes.iter().enumerate() {
            // Monotone finality per validator. Tuples compare lexicographically, so a
            // view restarting under a higher epoch is progress, not regression.
            if let Some(round) = node.finalized_round {
                if round < self.finalized_rounds[index] {
                    findings.push(Finding::RoundRegression {
                        validator: index,
                        from: self.finalized_rounds[index],
                        to: round,
                    });
                }
                self.finalized_rounds[index] = self.finalized_rounds[index].max(round);
                if round > self.tip_round {
                    self.tip_round = round;
                    self.tip_advanced_at = now;
                }
            }
            if let Some(height) = node.applied_height {
                if height < self.applied_heights[index] {
                    findings.push(Finding::FinalityRegression {
                        validator: index,
                        what: "applied height",
                        from: self.applied_heights[index],
                        to: height,
                    });
                }
                self.applied_heights[index] = self.applied_heights[index].max(height);
            }

            // Protocol fault evidence must never appear among honest validators.
            if node.evidence > self.evidence[index] {
                findings.push(Finding::FaultEvidence {
                    validator: index,
                    count: node.evidence,
                });
                self.evidence[index] = node.evidence;
            }

            // Nobody proposes blocks an honest peer must reject; a rejection
            // ticking here means someone's execution disagrees.
            if node.verify_invalid > self.verify_invalid[index] {
                findings.push(Finding::VerifyRejection {
                    validator: index,
                    count: node.verify_invalid,
                });
                self.verify_invalid[index] = node.verify_invalid;
            }

            // "Not running" is only ever expected while the driver holds a node
            // killed or stopped; a paused, partitioned, degraded, or healthy
            // validator that is not running died on its own.
            if !matches!(
                expectations.conditions[index],
                Condition::Killed | Condition::Stopped
            ) && node.running == Some(false)
            {
                findings.push(Finding::UnexpectedDeath { validator: index });
            }

            for line in &node.suspicious_log_lines {
                findings.push(Finding::SuspiciousLog {
                    validator: index,
                    line: line.clone(),
                });
            }
        }

        // Any two nodes serving different chain fingerprints are config drift.
        let fingerprints: Vec<(usize, String)> = poll
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                node.chain_fingerprint
                    .clone()
                    .filter(|fingerprint| !fingerprint.is_empty())
                    .map(|fingerprint| (index, fingerprint))
            })
            .collect();
        if fingerprints.windows(2).any(|pair| pair[0].1 != pair[1].1) {
            findings.push(Finding::ConfigDrift { fingerprints });
        }

        // Registry derivations, three failure directions: a node whose derivation
        // failed validation, a node whose derivation disagrees with its own config
        // schedule, and any two nodes deriving different committees for the same
        // epoch (each node's answer is a pure function of chain state — cross-node
        // disagreement is the class shadow mode exists to catch).
        let mut refused: Vec<(usize, String)> = Vec::new();
        let mut mismatched: Vec<(usize, String)> = Vec::new();
        let mut hashes_by_epoch: std::collections::BTreeMap<u64, Vec<(usize, String)>> =
            std::collections::BTreeMap::new();
        for (index, node) in poll.nodes.iter().enumerate() {
            let Some(registry) = &node.registry else {
                continue;
            };
            if registry.outcome == "carried_refused" {
                refused.push((
                    index,
                    format!(
                        "epoch {}: {}",
                        registry.last_epoch,
                        registry.refusal.as_deref().unwrap_or("(no reason served)"),
                    ),
                ));
            } else if !registry.matches_config {
                mismatched.push((
                    index,
                    format!(
                        "epoch {}: committee {} ({} members)",
                        registry.last_epoch, registry.committee_hash, registry.committee_size,
                    ),
                ));
            }
            hashes_by_epoch
                .entry(registry.last_epoch)
                .or_default()
                .push((
                    index,
                    format!("epoch {}: {}", registry.last_epoch, registry.committee_hash),
                ));
        }
        if !refused.is_empty() {
            findings.push(Finding::RegistryDrift {
                what: "registry derivation failed validation (rotation blocked)",
                details: refused,
            });
        }
        if !mismatched.is_empty() {
            findings.push(Finding::RegistryDrift {
                what: "registry derivation does not match the config schedule",
                details: mismatched,
            });
        }
        for details in hashes_by_epoch.into_values() {
            if details.windows(2).any(|pair| pair[0].1 != pair[1].1) {
                findings.push(Finding::RegistryDrift {
                    what: "nodes derive different committees for the same epoch",
                    details,
                });
            }
        }

        // Progress-without-quorum: once the settle margin after losing quorum has
        // passed, the finalized tip is frozen; any advance is a safety violation.
        // (Applied heights may keep draining already-finalized blocks — that is
        // catch-up, not progress.)
        if expectations.expect_liveness {
            self.forbidden_baseline = None;
        } else if now.duration_since(expectations.since) >= self.settle_margin {
            match self.forbidden_baseline {
                None => self.forbidden_baseline = Some(self.tip_round),
                Some(baseline) => {
                    if self.tip_round > baseline {
                        findings.push(Finding::ProgressWithoutQuorum {
                            baseline_round: baseline,
                            observed_round: self.tip_round,
                        });
                    }
                }
            }
        }

        // Liveness: expected live for a whole window, yet the tip never advanced.
        if expectations.expect_liveness
            && now.duration_since(expectations.since) >= self.liveness_window
            && now.duration_since(self.tip_advanced_at) >= self.liveness_window
        {
            findings.push(Finding::LivenessStall {
                window: self.liveness_window,
                at_round: self.tip_round,
                laggards: self
                    .finalized_rounds
                    .iter()
                    .enumerate()
                    .filter(|(_, round)| **round < self.tip_round)
                    .map(|(index, _)| index)
                    .collect(),
            });
            // Re-arm so a genuinely stuck cluster produces one finding per window,
            // not one per poll.
            self.tip_advanced_at = now;
        }

        // Settlement: the chain finalizes but L1 batch counters stand still, with
        // the settler believed healthy. Any sanctioned condition on the settler,
        // an L1 blackout, or a failed L1 probe excuses the lag (and restarts the
        // clocks, so a healed settler gets a full window to catch up).
        if let Some(observed) = &poll.settlement {
            if observed.executed_batches > observed.committed_batches {
                self.counter_insanity_streak += 1;
                if self.counter_insanity_streak >= 5 {
                    findings.push(Finding::SettlementProbeInsanity {
                        committed: observed.committed_batches,
                        executed: observed.executed_batches,
                    });
                    self.counter_insanity_streak = 0;
                }
            } else {
                self.counter_insanity_streak = 0;
            }
        }
        match &poll.settlement {
            Some(observed)
                if expectations.conditions[self.settler] == Condition::Healthy
                    && !expectations.l1_blackout =>
            {
                if observed.committed_batches > self.settlement_counters.0 {
                    self.settlement_counters.0 = observed.committed_batches;
                    self.committed_advanced_at = now;
                }
                if observed.executed_batches > self.settlement_counters.1 {
                    self.settlement_counters.1 = observed.executed_batches;
                    self.executed_advanced_at = now;
                }
                let chain_moved_since = |stuck_since: Instant| self.tip_advanced_at > stuck_since;
                if now.duration_since(self.committed_advanced_at) >= self.settlement_lag_window
                    && chain_moved_since(self.committed_advanced_at)
                {
                    findings.push(Finding::SettlementStall {
                        counter: "committed",
                        stuck_at: self.settlement_counters.0,
                        window: self.settlement_lag_window,
                    });
                    self.committed_advanced_at = now;
                }
                if now.duration_since(self.executed_advanced_at) >= self.settlement_lag_window
                    && chain_moved_since(self.executed_advanced_at)
                {
                    findings.push(Finding::SettlementStall {
                        counter: "executed",
                        stuck_at: self.settlement_counters.1,
                        window: self.settlement_lag_window,
                    });
                    self.executed_advanced_at = now;
                }
            }
            _ => {
                self.committed_advanced_at = now;
                self.executed_advanced_at = now;
            }
        }

        findings
    }
}

/// Log lines that always warrant a finding.
const FORBIDDEN_LOG_PATTERNS: [&str; 2] = ["panicked at", " ERROR "];

/// ERROR shapes that are the *expected* voice of an L1-facing component while
/// the driver holds the L1 dark: transport-level connectivity failures. A
/// panic, or an ERROR that is not connectivity-shaped, is a finding even then.
const L1_OUTAGE_TOLERATED_PATTERNS: [&str; 5] = [
    "error sending request",
    "connection refused",
    "tcp connect error",
    "operation timed out",
    "request timeout",
];

/// Whether a suspicious line is excused by an ongoing (or just-ended) L1
/// blackout: ERROR + connectivity-shaped, never panics.
pub fn is_tolerated_during_l1_outage(line: &str) -> bool {
    if line.contains("panicked at") {
        return false;
    }
    let lower = line.to_lowercase();
    L1_OUTAGE_TOLERATED_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

/// Log lines excused on the clean-logs check: benign teardown noise from a node the
/// driver itself stopped. Kept narrow so a novel critical-task panic still surfaces;
/// a real consensus failure cannot hide here, since safety violations arrive as
/// structured findings rather than log lines.
const ALLOWED_LOG_PATTERNS: [&str; 2] = [
    // On a graceful stop a straggler task can drop its tokio runtime from an async
    // context; the container still exits 0 and the chain keeps finalizing.
    // TODO: remove once the upstream runtime-drop straggler is fixed.
    "Cannot drop a runtime",
    "blocking/shutdown.rs",
];

pub fn is_suspicious_log_line(line: &str) -> bool {
    FORBIDDEN_LOG_PATTERNS
        .iter()
        .any(|pattern| line.contains(pattern))
        && !ALLOWED_LOG_PATTERNS
            .iter()
            .any(|pattern| line.contains(pattern))
}

/// Removes ANSI color escapes (`ESC [ ... m`) so the pattern matching above sees
/// the same text a human does. Container logs keep the node's colored output.
fn strip_ansi(line: &str) -> String {
    let mut cleaned = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            for skipped in chars.by_ref() {
                if skipped == 'm' {
                    break;
                }
            }
        } else {
            cleaned.push(character);
        }
    }
    cleaned
}

/// Runs a docker CLI command asynchronously with a hard timeout, returning its
/// stdout+stderr on success and `None` on failure/timeout. Every watcher probe
/// must stay non-blocking and bounded: a slow docker daemon may degrade a poll,
/// never the whole runtime.
async fn docker_output(args: &[&str], timeout: Duration) -> Option<String> {
    let child = tokio::process::Command::new("docker").args(args).output();
    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(output)) if output.status.success() => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            Some(text)
        }
        _ => None,
    }
}

/// The I/O half: polls one validator's endpoints and container state.
pub struct NodeProbe {
    pub container: String,
    pub status_url: String,
    pub rpc_url: String,
    pub metrics_url: String,
}

impl NodeProbe {
    pub fn from_manifest(manifest: &Manifest) -> Vec<NodeProbe> {
        manifest
            .validators
            .iter()
            .map(|validator| NodeProbe {
                container: format!("chaos-{}", validator.name),
                status_url: format!("http://127.0.0.1:{}/status", validator.host_status_port),
                rpc_url: format!("http://127.0.0.1:{}", validator.host_rpc_port),
                metrics_url: format!("http://127.0.0.1:{}/metrics", validator.host_metrics_port),
            })
            .collect()
    }

    /// Container state via docker; (running, paused).
    ///
    /// Async and bounded: the watcher used to shell out synchronously here, which
    /// parked tokio workers and — as container logs grew — eventually stalled the
    /// whole drive runtime (found by the overnight campaign).
    async fn container_state(&self) -> (Option<bool>, bool, Option<String>) {
        let output = docker_output(
            &[
                "inspect",
                "--format",
                "{{.State.Running}} {{.State.Paused}} {{.State.StartedAt}}",
                &self.container,
            ],
            Duration::from_secs(5),
        )
        .await;
        match output {
            Some(text) => {
                let mut parts = text.split_whitespace();
                let running = parts.next() == Some("true");
                let paused = parts.next() == Some("true");
                let started_at = parts.next().map(str::to_string);
                (Some(running), paused, started_at)
            }
            // The probe failed — the daemon was busy or the call timed out.
            // Unknown is not dead.
            None => (None, false, None),
        }
    }

    /// New log lines since `since` that match the forbidden patterns. `--tail`
    /// keeps the scan bounded no matter how old the container is.
    async fn suspicious_logs(
        &self,
        since: Instant,
        now: Instant,
        tolerate_l1_outage: bool,
    ) -> Vec<String> {
        let seconds = now.duration_since(since).as_secs().max(1);
        let Some(text) = docker_output(
            &[
                "logs",
                "--since",
                &format!("{seconds}s"),
                "--tail",
                "400",
                &self.container,
            ],
            Duration::from_secs(5),
        )
        .await
        else {
            return Vec::new();
        };
        let mut lines: Vec<String> = text
            .lines()
            .map(strip_ansi)
            .filter(|line| is_suspicious_log_line(line))
            .filter(|line| !(tolerate_l1_outage && is_tolerated_during_l1_outage(line)))
            .collect();
        lines.truncate(5); // one finding is enough; don't flood the artifacts
        lines
    }

    pub async fn observe(
        &self,
        client: &reqwest::Client,
        probe_height: Option<u64>,
        logs_since: Instant,
        now: Instant,
        tolerate_l1_outage: bool,
    ) -> NodeObservation {
        let (running, paused, started_at) = self.container_state().await;

        // All probes run concurrently: an unreachable node costs one client
        // timeout per poll, not a sum of them.
        let (status, block_probe, counters, suspicious_log_lines) = tokio::join!(
            self.status(client),
            async {
                match probe_height {
                    Some(height) if running == Some(true) && !paused => {
                        self.block_probe(client, height).await
                    }
                    _ => None,
                }
            },
            self.consensus_counters(client),
            self.suspicious_logs(logs_since, now, tolerate_l1_outage),
        );
        let (block_hash_at_probe, execution_at_probe) = match block_probe {
            Some((hash, print)) => (Some(hash), print),
            None => (None, None),
        };
        let (evidence, verify_invalid) = counters;
        let consensus = status.and_then(|status| status.consensus);
        let finalized_round = consensus.as_ref().and_then(|consensus| {
            consensus
                .finalized
                .as_ref()
                .map(|tip| (tip.epoch, tip.view))
        });
        let applied_height = consensus
            .as_ref()
            .and_then(|consensus| consensus.applied_height);
        let chain_fingerprint = consensus
            .as_ref()
            .map(|consensus| consensus.chain_fingerprint.clone());
        let registry = consensus
            .as_ref()
            .and_then(|consensus| consensus.registry.clone());

        NodeObservation {
            running,
            paused,
            started_at,
            finalized_round,
            applied_height,
            block_hash_at_probe,
            execution_at_probe,
            evidence,
            verify_invalid,
            chain_fingerprint,
            registry,
            suspicious_log_lines,
        }
    }

    async fn status(&self, client: &reqwest::Client) -> Option<StatusResponse> {
        match client.get(&self.status_url).send().await {
            Ok(response) => response.json().await.ok(),
            Err(_) => None,
        }
    }

    /// The probed block's hash plus, when it has transactions, an execution
    /// fingerprint: the full transaction-hash list and receipts for the first
    /// [`RECEIPT_SAMPLE`] of them (a deterministic sample — every node
    /// summarizes the same transactions).
    async fn block_probe(
        &self,
        client: &reqwest::Client,
        height: u64,
    ) -> Option<(String, Option<ExecutionFingerprint>)> {
        let response = self
            .rpc(
                client,
                "eth_getBlockByNumber",
                serde_json::json!([format!("0x{height:x}"), false]),
            )
            .await?;
        let hash = response["hash"].as_str()?.to_string();
        let txs: Vec<String> = response["transactions"]
            .as_array()?
            .iter()
            .filter_map(|tx| tx.as_str().map(|hash| hash.to_string()))
            .collect();
        if txs.is_empty() {
            return Some((hash, None));
        }

        let mut receipts = Vec::new();
        for tx in txs.iter().take(RECEIPT_SAMPLE) {
            let receipt = self
                .rpc(client, "eth_getTransactionReceipt", serde_json::json!([tx]))
                .await?;
            receipts.push((
                tx.clone(),
                format!(
                    "{}|{}|{}",
                    receipt["status"].as_str().unwrap_or("?"),
                    receipt["gasUsed"].as_str().unwrap_or("?"),
                    receipt["logsBloom"].as_str().unwrap_or("?"),
                ),
            ));
        }
        Some((
            hash,
            Some(ExecutionFingerprint {
                txs: txs.join(","),
                receipts,
            }),
        ))
    }

    async fn rpc(
        &self,
        client: &reqwest::Client,
        method: &str,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let response: serde_json::Value = client
            .post(&self.rpc_url)
            .json(&request)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        (!response["result"].is_null()).then(|| response["result"].clone())
    }

    /// Two counter families from one metrics scrape: the summed protocol
    /// fault-evidence counters, and the permanent verify rejections.
    async fn consensus_counters(&self, client: &reqwest::Client) -> (u64, u64) {
        let Ok(response) = client.get(&self.metrics_url).send().await else {
            return (0, 0);
        };
        let Ok(text) = response.text().await else {
            return (0, 0);
        };
        let sum = |predicate: &dyn Fn(&str) -> bool| -> u64 {
            text.lines()
                .filter(|line| predicate(line))
                .filter_map(|line| line.split_whitespace().last())
                .filter_map(|value| value.parse::<f64>().ok())
                .sum::<f64>() as u64
        };
        let evidence = sum(&|line: &str| {
            line.starts_with("consensus_activity")
                && (line.contains("conflicting_notarize")
                    || line.contains("conflicting_finalize")
                    || line.contains("nullify_finalize"))
        });
        let verify_invalid = sum(&|line: &str| {
            line.starts_with("consensus_verify_verdicts") && line.contains("\"invalid\"")
        });
        (evidence, verify_invalid)
    }
}

/// Reads the chain's settlement counters straight from L1 — the reading must
/// not depend on any node, least of all the settler it is judging.
pub struct SettlementProbe {
    pub l1_url: String,
    pub diamond: String,
}

/// Everything the watcher needs to judge settlement: where to read L1, who the
/// settler is, and how long the counters may stand still.
pub struct SettlementWatch {
    pub probe: SettlementProbe,
    pub settler: usize,
    pub lag_window: Duration,
}

impl SettlementProbe {
    pub async fn observe(&self, client: &reqwest::Client) -> Option<SettlementObservation> {
        // IZKChain selectors (`cast sig`): getTotalBatchesCommitted() = 0xdb1f0bf9,
        // getTotalBatchesExecuted() = 0xb8c2f66f. These were originally swapped,
        // which mislabeled every settlement finding (a frozen *execute* train read
        // as frozen commits and vice versa) — caught by cross-checking a finding
        // against the settler's own sender log.
        let committed = self.counter(client, "0xdb1f0bf9").await?;
        let executed = self.counter(client, "0xb8c2f66f").await?;
        Some(SettlementObservation {
            committed_batches: committed,
            executed_batches: executed,
        })
    }

    async fn counter(&self, client: &reqwest::Client, selector: &str) -> Option<u64> {
        let response: serde_json::Value = client
            .post(&self.l1_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "eth_call",
                "params": [{"to": self.diamond, "data": selector}, "latest"],
            }))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let hex = response.get("result")?.as_str()?;
        u64::from_str_radix(hex.trim_start_matches("0x").trim_start_matches('0'), 16)
            .ok()
            .or(Some(0))
    }
}

/// How often the watcher polls the cluster.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Receipts fetched per node per poll for the execution fingerprint.
const RECEIPT_SAMPLE: usize = 3;

/// The watcher loop: polls every validator, feeds the [`Checker`], and sends the
/// first non-empty batch of findings (with the poll that produced it) before
/// returning. Sending once and stopping is deliberate — the driver freezes the
/// experiment on the first finding, so there is nothing left to watch.
/// The watcher's tuning and output knobs, separated from its wiring (probes,
/// channels) so the signature stays readable as knobs accumulate.
pub struct WatchOptions {
    pub settle_margin: Duration,
    pub liveness_window: Duration,
    /// Per-poll metrics sink (JSONL); `None` disables the stream.
    pub metrics: Option<std::fs::File>,
    /// See the drive flag of the same name: freezing on SettlementStall is
    /// off while the known prover-job strand is with the team.
    pub fail_on_settlement_stall: bool,
}

pub async fn watch(
    probes: Vec<NodeProbe>,
    settlement: Option<SettlementWatch>,
    expectations: tokio::sync::watch::Receiver<Expectations>,
    findings: tokio::sync::mpsc::Sender<(Poll, Vec<Finding>)>,
    options: WatchOptions,
) {
    let WatchOptions {
        settle_margin,
        liveness_window,
        mut metrics,
        fail_on_settlement_stall,
    } = options;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .expect("default reqwest client");
    let mut checker = Checker::new(
        probes.len(),
        settle_margin,
        liveness_window,
        settlement.as_ref().map(|watch| watch.settler).unwrap_or(0),
        settlement
            .as_ref()
            .map(|watch| watch.lag_window)
            .unwrap_or(Duration::from_secs(120)),
    );
    let settlement_probe = settlement.map(|watch| watch.probe);
    // The height probed for agreement: the lowest applied height reported in the
    // *previous* poll, so every reachable validator has the block by now.
    let mut probe_height: Option<u64> = None;
    let started = Instant::now();
    let mut logs_since = Instant::now();
    // Log lines written during a blackout can surface in the poll after it
    // ended; keep excusing connectivity noise for a short grace period.
    let mut l1_tolerance_until = Instant::now() - Duration::from_secs(1);

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let now = Instant::now();
        let current_expectations = expectations.borrow().clone();
        if current_expectations.l1_blackout {
            l1_tolerance_until = now + Duration::from_secs(15);
        }
        let tolerate_l1_outage = current_expectations.l1_blackout || now < l1_tolerance_until;

        let (observations, settlement) = tokio::join!(
            futures::future::join_all(probes.iter().map(|probe| {
                probe.observe(&client, probe_height, logs_since, now, tolerate_l1_outage)
            })),
            async {
                match &settlement_probe {
                    Some(probe) => probe.observe(&client).await,
                    None => None,
                }
            },
        );
        logs_since = now;

        let poll = Poll {
            probe_height,
            nodes: observations,
            settlement,
        };
        probe_height = poll
            .nodes
            .iter()
            .filter_map(|node| node.applied_height)
            .min();

        // The metrics stream: one line per poll, everything the poll saw plus
        // the driver's beliefs at that moment. Post-run analysis derives block
        // rates, finality latency, idle cadence, and view efficiency from the
        // time series; a write failure (disk full) degrades to no metrics, never
        // to a dead watcher. The write is a small local append — microseconds,
        // nothing like the blocking docker calls that once stalled the runtime.
        if let Some(file) = metrics.as_mut() {
            let line = serde_json::json!({
                "elapsed_ms": now.duration_since(started).as_millis() as u64,
                "expect_liveness": current_expectations.expect_liveness,
                "l1_blackout": current_expectations.l1_blackout,
                "conditions": current_expectations.conditions,
                "poll": &poll,
            });
            let _ = writeln!(file, "{line}");
        }

        let batch = checker.observe(now, &current_expectations, &poll);
        // SettlementStall is currently a *known* node bug (a leaked prover-job
        // assignment freezes the execute train for the full snark_job_timeout —
        // consensus_planning/soak-overnight2/INVESTIGATION.md §2, fix with the
        // team). It is chain-safe, so by default a stall is recorded — stdout
        // and the metrics stream — without freezing the experiment; the flag
        // restores freezing once the node fix lands. Every other finding still
        // freezes, including SettlementProbeInsanity (a rig self-check).
        let (fatal, tolerated): (Vec<_>, Vec<_>) = batch.into_iter().partition(|finding| {
            fail_on_settlement_stall || !matches!(finding, Finding::SettlementStall { .. })
        });
        for finding in &tolerated {
            println!("tolerated finding (not freezing): {finding:?}");
            if let Some(file) = metrics.as_mut() {
                let line = serde_json::json!({
                    "elapsed_ms": now.duration_since(started).as_millis() as u64,
                    "tolerated_finding": format!("{finding:?}"),
                });
                let _ = writeln!(file, "{line}");
            }
        }
        if !fatal.is_empty() {
            let _ = findings.send((poll, fatal)).await;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_expectations(validators: usize) -> Expectations {
        Expectations {
            conditions: vec![Condition::Healthy; validators],
            expect_liveness: true,
            since: Instant::now() - Duration::from_secs(3600),
            l1_blackout: false,
        }
    }

    /// An observation in epoch 0 — the view doubles as the applied height.
    fn observation(finalized_view: u64, hash: &str) -> NodeObservation {
        NodeObservation {
            running: Some(true),
            paused: false,
            started_at: None,
            finalized_round: Some((0, finalized_view)),
            applied_height: Some(finalized_view),
            block_hash_at_probe: Some(hash.to_string()),
            execution_at_probe: None,
            evidence: 0,
            verify_invalid: 0,
            chain_fingerprint: None,
            registry: None,
            suspicious_log_lines: Vec::new(),
        }
    }

    #[test]
    fn agreement_violation_is_a_finding() {
        let mut checker = Checker::new(
            2,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let poll = Poll {
            settlement: None,
            probe_height: Some(7),
            nodes: vec![observation(10, "0xaaaa"), observation(10, "0xbbbb")],
        };
        let findings = checker.observe(Instant::now(), &healthy_expectations(2), &poll);
        assert!(matches!(
            findings[0],
            Finding::HashDisagreement { height: 7, .. }
        ));
    }

    #[test]
    fn progress_without_quorum_is_a_finding_after_the_settle_margin() {
        let mut checker = Checker::new(
            3,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let now = Instant::now();
        let mut expectations = healthy_expectations(3);
        let poll = |view| Poll {
            settlement: None,
            probe_height: None,
            nodes: vec![observation(view, "0x"); 3],
        };

        checker.observe(now, &expectations, &poll(20));

        // Quorum lost; certificates in flight may still land inside the margin.
        expectations.expect_liveness = false;
        expectations.since = now;
        let inside_margin = now + Duration::from_secs(2);
        assert!(
            checker
                .observe(inside_margin, &expectations, &poll(21))
                .is_empty()
        );

        // The margin passed: the tip is frozen at 21; any further advance is fatal.
        let after_margin = now + Duration::from_secs(6);
        assert!(
            checker
                .observe(after_margin, &expectations, &poll(21))
                .is_empty()
        );
        let findings = checker.observe(now + Duration::from_secs(8), &expectations, &poll(25));
        assert!(matches!(
            findings[0],
            Finding::ProgressWithoutQuorum {
                baseline_round: (0, 21),
                observed_round: (0, 25)
            }
        ));
    }

    #[test]
    fn liveness_stall_fires_once_per_window() {
        let mut checker = Checker::new(
            2,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let start = Instant::now();
        let expectations = healthy_expectations(2);
        let poll = Poll {
            settlement: None,
            probe_height: None,
            nodes: vec![observation(5, "0x"); 2],
        };

        checker.observe(start, &expectations, &poll);
        assert!(
            checker
                .observe(start + Duration::from_secs(30), &expectations, &poll)
                .is_empty()
        );
        let findings = checker.observe(start + Duration::from_secs(3601), &expectations, &poll);
        assert!(matches!(findings[0], Finding::LivenessStall { .. }));
        // Immediately after, the window re-arms — no duplicate finding.
        assert!(
            checker
                .observe(start + Duration::from_secs(3602), &expectations, &poll)
                .is_empty()
        );
    }

    /// A poll with the given chain view and settlement counters.
    fn settlement_poll(view: u64, committed: u64, executed: u64) -> Poll {
        Poll {
            probe_height: None,
            nodes: vec![observation(view, "0x"), observation(view, "0x")],
            settlement: Some(SettlementObservation {
                committed_batches: committed,
                executed_batches: executed,
            }),
        }
    }

    #[test]
    fn settlement_stall_with_a_healthy_settler_is_a_finding() {
        let mut checker = Checker::new(
            2,
            Duration::from_secs(5),
            Duration::from_secs(3600),
            0,
            Duration::from_secs(10),
        );
        let expectations = healthy_expectations(2);
        let start = Instant::now();

        // Settlement lands a batch, then freezes while the chain keeps finalizing.
        assert!(
            checker
                .observe(start, &expectations, &settlement_poll(10, 1, 1))
                .is_empty()
        );
        assert!(
            checker
                .observe(
                    start + Duration::from_secs(6),
                    &expectations,
                    &settlement_poll(11, 1, 1),
                )
                .is_empty(),
            "within the window nothing fires",
        );
        let findings = checker.observe(
            start + Duration::from_secs(12),
            &expectations,
            &settlement_poll(12, 1, 1),
        );
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                Finding::SettlementStall {
                    counter: "committed",
                    ..
                }
            )),
            "got: {findings:?}",
        );
    }

    #[test]
    fn settlement_lag_is_excused_while_the_settler_is_down() {
        let mut checker = Checker::new(
            2,
            Duration::from_secs(5),
            Duration::from_secs(3600),
            0,
            Duration::from_secs(10),
        );
        let healthy = healthy_expectations(2);
        let mut settler_down = healthy_expectations(2);
        settler_down.conditions[0] = Condition::Killed;
        let start = Instant::now();

        checker.observe(start, &healthy, &settlement_poll(10, 1, 1));
        // The driver kills the settler: the frozen counters are sanctioned, however
        // long it stays down.
        assert!(
            checker
                .observe(
                    start + Duration::from_secs(20),
                    &settler_down,
                    &settlement_poll(11, 1, 1),
                )
                .is_empty(),
        );
        // Healed: the clock restarts — a full window of grace before judgment.
        assert!(
            checker
                .observe(
                    start + Duration::from_secs(22),
                    &healthy,
                    &settlement_poll(12, 1, 1),
                )
                .is_empty(),
        );
        // Still frozen a full window later, settler healthy throughout: finding.
        let findings = checker.observe(
            start + Duration::from_secs(33),
            &healthy,
            &settlement_poll(13, 1, 1),
        );
        assert!(
            findings
                .iter()
                .any(|finding| matches!(finding, Finding::SettlementStall { .. })),
            "got: {findings:?}",
        );
    }

    #[test]
    fn regression_and_evidence_and_death_are_findings() {
        let mut checker = Checker::new(
            1,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let expectations = healthy_expectations(1);
        let now = Instant::now();

        checker.observe(
            now,
            &expectations,
            &Poll {
                settlement: None,
                probe_height: None,
                nodes: vec![observation(10, "0x")],
            },
        );

        let mut regressed = observation(4, "0x");
        regressed.evidence = 1;
        regressed.running = Some(false);
        let findings = checker.observe(
            now,
            &expectations,
            &Poll {
                settlement: None,
                probe_height: None,
                nodes: vec![regressed],
            },
        );
        assert!(
            findings
                .iter()
                .any(|finding| matches!(finding, Finding::RoundRegression { .. }))
        );
        assert!(
            findings
                .iter()
                .any(|finding| matches!(finding, Finding::FaultEvidence { .. }))
        );
        assert!(
            findings
                .iter()
                .any(|finding| matches!(finding, Finding::UnexpectedDeath { .. }))
        );
    }

    #[test]
    fn a_failed_container_probe_is_not_a_death() {
        // A busy docker daemon times out inspects — for every container at once,
        // under restart churn. Unknown state must never read as a death; a real
        // one is confirmed by the next successful poll.
        let mut checker = Checker::new(
            1,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let expectations = healthy_expectations(1);
        let mut unknown = observation(0, "0x");
        unknown.running = None;
        let findings = checker.observe(
            Instant::now(),
            &expectations,
            &Poll {
                settlement: None,
                probe_height: None,
                nodes: vec![unknown],
            },
        );
        assert!(
            !findings
                .iter()
                .any(|finding| matches!(finding, Finding::UnexpectedDeath { .. })),
            "a probe failure produced a death finding: {findings:?}"
        );
    }

    fn print(txs: &str, receipt: &str) -> ExecutionFingerprint {
        ExecutionFingerprint {
            txs: txs.to_string(),
            receipts: vec![("0xt0".to_string(), receipt.to_string())],
        }
    }

    #[test]
    fn matching_hashes_with_diverging_receipts_is_a_finding() {
        // The scenario hash agreement cannot see: everyone serves the same
        // block hash, but one node's receipt for a sampled transaction differs
        // — an execution/storage/RPC divergence.
        let mut checker = Checker::new(
            2,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let mut a = observation(10, "0xsame");
        let mut b = observation(10, "0xsame");
        a.execution_at_probe = Some(print("0xt0", "0x1|0x5208|0x00"));
        b.execution_at_probe = Some(print("0xt0", "0x0|0x5208|0x00"));
        let findings = checker.observe(
            Instant::now(),
            &healthy_expectations(2),
            &Poll {
                settlement: None,
                probe_height: Some(7),
                nodes: vec![a, b],
            },
        );
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                Finding::ExecutionDisagreement {
                    height: 7,
                    what: "sampled receipts",
                    ..
                }
            )),
            "no execution finding: {findings:?}"
        );
    }

    #[test]
    fn diverging_config_fingerprints_are_a_finding() {
        let mut checker = Checker::new(
            2,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let mut a = observation(10, "0x");
        let mut b = observation(10, "0x");
        a.chain_fingerprint = Some("aaaa".to_string());
        b.chain_fingerprint = Some("bbbb".to_string());
        let findings = checker.observe(
            Instant::now(),
            &healthy_expectations(2),
            &Poll {
                settlement: None,
                probe_height: None,
                nodes: vec![a, b],
            },
        );
        assert!(
            findings
                .iter()
                .any(|finding| matches!(finding, Finding::ConfigDrift { .. })),
            "no drift finding: {findings:?}"
        );
    }

    /// A healthy registry observation: derived, matching, one agreed hash.
    fn registry_observation(epoch: u64, hash: &str) -> zksync_os_status_server::RegistryStatus {
        zksync_os_status_server::RegistryStatus {
            mode: "shadow".to_string(),
            last_epoch: epoch,
            last_lookahead_height: epoch * 100,
            outcome: "derived".to_string(),
            matches_config: true,
            refusal: None,
            committee_hash: hash.to_string(),
            committee_size: 4,
        }
    }

    #[test]
    fn registry_drift_fires_on_mismatch_refusal_and_cross_node_divergence_only() {
        let check = |registries: Vec<Option<zksync_os_status_server::RegistryStatus>>| {
            let mut checker = Checker::new(
                registries.len(),
                Duration::from_secs(5),
                Duration::from_secs(60),
                0,
                Duration::from_secs(120),
            );
            let nodes = registries
                .into_iter()
                .map(|registry| {
                    let mut node = observation(10, "0x");
                    node.registry = registry;
                    node
                })
                .collect();
            checker
                .observe(
                    Instant::now(),
                    &healthy_expectations(2),
                    &Poll {
                        settlement: None,
                        probe_height: None,
                        nodes,
                    },
                )
                .into_iter()
                .filter(|finding| matches!(finding, Finding::RegistryDrift { .. }))
                .collect::<Vec<_>>()
        };

        // Healthy: both derive the same committee and match their configs.
        let clean = check(vec![
            Some(registry_observation(5, "aaaa")),
            Some(registry_observation(5, "aaaa")),
        ]);
        assert!(clean.is_empty(), "healthy shadow must not fire: {clean:?}");

        // Nodes at different epochs (one derived ahead) is routine, not drift.
        let staggered = check(vec![
            Some(registry_observation(5, "aaaa")),
            Some(registry_observation(6, "bbbb")),
        ]);
        assert!(
            staggered.is_empty(),
            "staggered epochs are routine: {staggered:?}"
        );

        // A node whose derivation disagrees with its own config.
        let mut mismatched = registry_observation(5, "cccc");
        mismatched.matches_config = false;
        let findings = check(vec![
            Some(registry_observation(5, "aaaa")),
            Some(mismatched),
        ]);
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                Finding::RegistryDrift { what, .. } if what.contains("does not match")
            )),
            "config mismatch must fire: {findings:?}"
        );
        // ... and the same poll also shows two nodes disagreeing for epoch 5.
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                Finding::RegistryDrift { what, .. } if what.contains("same epoch")
            )),
            "cross-node divergence must fire: {findings:?}"
        );

        // A refused derivation is a finding on its own (rotation is blocked).
        let mut refused = registry_observation(5, "aaaa");
        refused.outcome = "carried_refused".to_string();
        refused.refusal = Some("identity 2 carries an invalid proof of possession".to_string());
        let findings = check(vec![Some(registry_observation(5, "aaaa")), Some(refused)]);
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                Finding::RegistryDrift { what, .. } if what.contains("failed validation")
            )),
            "refusal must fire: {findings:?}"
        );

        // Nodes without a registry section (schedule mode) contribute nothing.
        let absent = check(vec![None, Some(registry_observation(5, "aaaa"))]);
        assert!(
            absent.is_empty(),
            "absent sections are not drift: {absent:?}"
        );
    }

    #[test]
    fn a_verify_rejection_is_a_finding() {
        let mut checker = Checker::new(
            2,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let mut node = observation(10, "0x");
        node.verify_invalid = 1;
        let findings = checker.observe(
            Instant::now(),
            &healthy_expectations(2),
            &Poll {
                settlement: None,
                probe_height: None,
                nodes: vec![observation(10, "0x"), node],
            },
        );
        assert!(
            findings.iter().any(|finding| matches!(
                finding,
                Finding::VerifyRejection {
                    validator: 1,
                    count: 1
                }
            )),
            "no rejection finding: {findings:?}"
        );
    }

    #[test]
    fn a_restarted_node_may_re_report_older_rounds() {
        // With epoch retention, a restarted validator replays from its oldest
        // retained epoch and its /status reports that older round until it
        // catches up. After a Killed→Healthy transition that dip is expected;
        // the same dip on a node that never restarted stays a finding.
        let mut checker = Checker::new(
            1,
            Duration::from_secs(5),
            Duration::from_secs(600),
            0,
            Duration::from_secs(120),
        );
        let mut killed = healthy_expectations(1);
        killed.conditions[0] = Condition::Killed;
        let now = Instant::now();

        let at = |view, hash: &str| Poll {
            settlement: None,
            probe_height: None,
            nodes: vec![observation(view, hash)],
        };
        assert!(
            checker
                .observe(now, &healthy_expectations(1), &at(46, "0x"))
                .is_empty()
        );
        assert!(checker.observe(now, &killed, &at(46, "0x")).is_empty());
        // Restarted, replaying: an older round appears. Not a finding.
        let findings = checker.observe(now, &healthy_expectations(1), &at(19, "0x"));
        assert!(
            !findings
                .iter()
                .any(|finding| matches!(finding, Finding::RoundRegression { .. })),
            "restart replay flagged as a regression: {findings:?}"
        );
        // No restart in between: the same dip is a real finding.
        assert!(
            checker
                .observe(now, &healthy_expectations(1), &at(50, "0x"))
                .is_empty()
        );
        let findings = checker.observe(now, &healthy_expectations(1), &at(20, "0x"));
        assert!(
            findings
                .iter()
                .any(|finding| matches!(finding, Finding::RoundRegression { .. })),
            "a genuine regression was not flagged: {findings:?}"
        );
    }

    #[test]
    fn a_liveness_stall_names_its_laggards() {
        let mut checker = Checker::new(
            3,
            Duration::from_secs(5),
            Duration::from_secs(1),
            0,
            Duration::from_secs(120),
        );
        let mut expectations = healthy_expectations(3);
        expectations.since = Instant::now() - Duration::from_secs(120);
        // Validator 2 sits behind the tip the other two reached.
        let poll = Poll {
            settlement: None,
            probe_height: None,
            nodes: vec![
                observation(10, "0x"),
                observation(10, "0x"),
                observation(4, "0x"),
            ],
        };
        let _ = checker.observe(
            Instant::now() - Duration::from_secs(60),
            &expectations,
            &poll,
        );
        let findings = checker.observe(Instant::now(), &expectations, &poll);
        let stall = findings
            .iter()
            .find_map(|finding| match finding {
                Finding::LivenessStall { laggards, .. } => Some(laggards.clone()),
                _ => None,
            })
            .expect("no stall finding");
        assert_eq!(stall, vec![2]);
    }

    #[test]
    fn view_restart_at_an_epoch_boundary_is_not_a_regression() {
        let mut checker = Checker::new(
            1,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let expectations = healthy_expectations(1);
        let now = Instant::now();
        let poll = |epoch, view| Poll {
            settlement: None,
            probe_height: None,
            nodes: vec![NodeObservation {
                finalized_round: Some((epoch, view)),
                ..observation(0, "0x")
            }],
        };

        // Late in epoch 1 the committee finalizes at view 703; the first
        // finalizations of epoch 2 carry tiny views. That is a handoff, not a
        // rollback — exactly what an epoch-blind view comparison misreads.
        checker.observe(now, &expectations, &poll(1, 703));
        assert!(checker.observe(now, &expectations, &poll(2, 3)).is_empty());

        // Within one epoch, a lower view is still a real regression.
        let findings = checker.observe(now, &expectations, &poll(2, 1));
        assert!(matches!(
            findings[0],
            Finding::RoundRegression {
                from: (2, 3),
                to: (2, 1),
                ..
            }
        ));
    }

    #[test]
    fn applied_height_baseline_resets_after_a_restart_heal() {
        let mut checker = Checker::new(
            1,
            Duration::from_secs(5),
            Duration::from_secs(60),
            0,
            Duration::from_secs(120),
        );
        let now = Instant::now();
        let poll = |applied| Poll {
            settlement: None,
            probe_height: None,
            nodes: vec![NodeObservation {
                applied_height: Some(applied),
                ..observation(50, "0x")
            }],
        };

        // Healthy progress to applied height 40.
        checker.observe(now, &healthy_expectations(1), &poll(40));

        // Killed, then healed: the restarted node replays from its own watermark, so
        // a lower applied height right after the heal is legitimate...
        let mut expectations = healthy_expectations(1);
        expectations.conditions[0] = Condition::Killed;
        checker.observe(
            now,
            &expectations,
            &Poll {
                settlement: None,
                probe_height: None,
                nodes: vec![NodeObservation {
                    running: Some(false),
                    finalized_round: None,
                    applied_height: None,
                    block_hash_at_probe: None,
                    ..observation(0, "0x")
                }],
            },
        );
        expectations.conditions[0] = Condition::Healthy;
        assert!(checker.observe(now, &expectations, &poll(10)).is_empty());

        // ...but with no restart in between, the same regression is a finding.
        let findings = checker.observe(now, &expectations, &poll(3));
        assert!(matches!(
            findings[0],
            Finding::FinalityRegression {
                what: "applied height",
                from: 10,
                to: 3,
                ..
            }
        ));
    }

    #[test]
    fn a_changed_container_start_time_resets_baselines() {
        // The condition-transition reset requires a poll to *observe* the
        // Killed window; under storm cadence it can miss one entirely. A
        // changed .State.StartedAt proves the restart regardless.
        let mut checker = Checker::new(
            1,
            Duration::from_secs(5),
            Duration::from_secs(600),
            0,
            Duration::from_secs(120),
        );
        let expectations = healthy_expectations(1);
        let now = Instant::now();
        let at = |view, started: &str| Poll {
            settlement: None,
            probe_height: None,
            nodes: vec![NodeObservation {
                started_at: Some(started.to_string()),
                applied_height: Some(view),
                ..observation(view, "0x")
            }],
        };

        // Healthy progress under one incarnation...
        assert!(
            checker
                .observe(now, &expectations, &at(8076, "2026-07-17T09:40:00Z"))
                .is_empty()
        );
        // ...then a lower applied height under a NEW start time: a restart
        // replay, not a regression — even though the driver's conditions
        // never showed Killed.
        assert!(
            checker
                .observe(now, &expectations, &at(7950, "2026-07-17T09:43:10Z"))
                .is_empty(),
            "replay after an unobserved restart flagged as a regression",
        );
        // The same dip with an UNCHANGED start time is a real finding.
        checker.observe(now, &expectations, &at(8100, "2026-07-17T09:43:10Z"));
        let findings = checker.observe(now, &expectations, &at(7000, "2026-07-17T09:43:10Z"));
        assert!(
            findings
                .iter()
                .any(|finding| matches!(finding, Finding::FinalityRegression { .. })),
            "a genuine regression was not flagged: {findings:?}",
        );
    }

    #[test]
    fn persistent_executed_above_committed_is_probe_insanity() {
        // On-chain, executes can never lead commits; a persistent excess means
        // the probe is miswired (the selector swap this check exists for).
        // Brief excess is tolerated: the two counters come from separate,
        // non-atomic eth_calls.
        let mut checker = Checker::new(
            2,
            Duration::from_secs(5),
            Duration::from_secs(3600),
            0,
            Duration::from_secs(3600),
        );
        let expectations = healthy_expectations(2);
        let now = Instant::now();

        // A short-lived race (2 polls) never fires.
        for _ in 0..2 {
            let findings = checker.observe(now, &expectations, &settlement_poll(10, 5, 6));
            assert!(
                findings.is_empty(),
                "raced counters must not fire: {findings:?}"
            );
        }
        checker.observe(now, &expectations, &settlement_poll(11, 7, 6));

        // Five consecutive violating polls: the probe is miswired.
        let mut fired = false;
        for _ in 0..5 {
            let findings = checker.observe(now, &expectations, &settlement_poll(12, 5, 6));
            fired |= findings
                .iter()
                .any(|finding| matches!(finding, Finding::SettlementProbeInsanity { .. }));
        }
        assert!(fired, "a persistent inversion never fired");
    }

    #[test]
    fn log_filter_allows_known_shutdown_noise() {
        // A plain panic, or any ERROR line, is suspicious.
        assert!(is_suspicious_log_line(
            "thread 'main' panicked at lib/x.rs:1:1"
        ));
        assert!(is_suspicious_log_line("2026-07-04 ERROR something new"));

        // The tokio runtime-drop straggler on a graceful stop is tolerated (see
        // ALLOWED_LOG_PATTERNS): both its error line and its panic site are excused.
        assert!(!is_suspicious_log_line(
            "2026-07-04 ERROR commonware_runtime::utils::handle: task panicked \
             err=\"Cannot drop a runtime in a context where blocking is not allowed\""
        ));
        assert!(!is_suspicious_log_line(
            "thread 'tokio-rt-worker' (215) panicked at /usr/local/cargo/registry/\
             src/index.crates.io-xxx/tokio-1.52.3/src/runtime/blocking/shutdown.rs:51:21:"
        ));

        // A pipeline segment failing while the node is running (not shutting down)
        // must surface.
        assert!(is_suspicious_log_line(
            "thread 'tokio-rt-worker' (7) panicked at lib/pipeline/src/builder.rs:131:41: \
             pipeline segment failed: consumer is catastrophically behind"
        ));

        // A novel critical-task panic is not tolerated.
        assert!(is_suspicious_log_line(
            "2026-07-04 ERROR reth_tasks::runtime: Critical task `sequencer` panicked: `index out of bounds`"
        ));
    }

    #[test]
    fn ansi_escapes_are_stripped_before_matching() {
        // tracing renders the level as `<ESC>[31mERROR<ESC>[0m` — without stripping,
        // the ` ERROR ` pattern never matches colored container logs.
        let colored = "2026-07-04T15:00:00Z \u{1b}[31mERROR\u{1b}[0m zksync_os_sequencer: boom";
        assert!(!is_suspicious_log_line(colored));
        assert_eq!(
            strip_ansi(colored),
            "2026-07-04T15:00:00Z ERROR zksync_os_sequencer: boom"
        );
        assert!(is_suspicious_log_line(&strip_ansi(colored)));
    }
}
