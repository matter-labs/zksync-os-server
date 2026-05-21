use crate::model::ConsensusRole;
use crate::status::RaftConsensusStatus;
use openraft::error::{CheckIsLeaderError, RaftError};
use openraft::{Raft, ServerState};
use reth_network_peers::PeerId;
use reth_tasks::Runtime;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval, timeout};
use zksync_os_consensus_types::{RaftNode, RaftTypeConfig};

/// How often we re-probe `ensure_linearizable` while holding the Leader state but waiting
/// for confirmation. We still wake on every OpenRaft metrics change (to keep the status
/// watch and the role-loss reaction responsive), but the probe call itself is rate-limited
/// to one per `PROBE_INTERVAL` — without this, a metrics-churn storm against unreachable
/// voters produces one openraft-side ERROR (`timeout while confirming leadership for read
/// request`) per unreachable voter per metrics tick. The `claims_leader: false → true` edge
/// bypasses the rate limit so a fresh election confirms without paying the full interval.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Per-probe budget for the linearizability round-trip.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// While confirmation keeps failing with the same cause, re-emit the log at most this often
/// so a stuck cluster keeps a "still degraded" reminder in the log without flooding it.
const STUCK_REMINDER_INTERVAL: Duration = Duration::from_secs(30);

type LinearizableErr = RaftError<PeerId, CheckIsLeaderError<PeerId, RaftNode>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProbeFailure {
    /// This node has been deposed; another node is now leader. Routine after a clean
    /// failover or on a rejoining stale leader — informational, not an alarm.
    ForwardToLeader,
    /// Could not collect a quorum ack within the probe timeout. The cluster cannot make
    /// progress; this is the operational alarm condition.
    QuorumNotEnough,
    /// Probe call did not return within `PROBE_TIMEOUT`. Usually indicates the same problem
    /// as `QuorumNotEnough` but caught by our local timer.
    Timeout,
    /// `Raft` task is no longer running. Typically a fatal startup or shutdown condition.
    Fatal,
}

struct FailureStreak {
    kind: ProbeFailure,
    started_at: Instant,
    last_logged_at: Instant,
}

/// Spawns a background task that translates OpenRaft metrics into two node-facing signals:
/// a coarse `ConsensusRole` watch channel used by the sequencer, and a richer
/// `RaftConsensusStatus` watch channel exposed by the status server.
///
/// OpenRaft may briefly report `Leader` while a node is still replaying committed entries after
/// an election. To avoid producing blocks too early, this monitor only upgrades the node to
/// `ConsensusRole::Leader` after `ensure_linearizable()` succeeds within a short timeout.
/// If the node steps down or the confirmation probe fails, the role falls back to `Replica`.
///
/// The task exits automatically when the OpenRaft metrics channel closes or when all receivers
/// for both output watch channels are dropped.
pub fn spawn_leadership_monitor(
    runtime: &Runtime,
    raft: Raft<RaftTypeConfig>,
    node_id_str: String,
    leader_tx: watch::Sender<ConsensusRole>,
    status_tx: watch::Sender<Option<RaftConsensusStatus>>,
) {
    let mut metrics_rx = raft.metrics();
    runtime.spawn_critical_task("raft leadership monitor", async move {
        let mut last_metrics_key = None;
        let mut leader_confirmed = false;
        let mut prev_role = ConsensusRole::Replica;
        let mut streak: Option<FailureStreak> = None;
        let mut last_probe_at: Option<Instant> = None;
        let mut probe_timer = interval(PROBE_INTERVAL);
        probe_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            // Wake on either an OpenRaft metrics change or the periodic probe tick. Both
            // are needed: metrics changes drive the status watch and the role-loss
            // reaction; the probe tick ensures we periodically re-attempt confirmation
            // even if metrics go quiet. The `ensure_linearizable` call below is separately
            // rate-limited by `last_probe_at` — see `PROBE_INTERVAL`.
            tokio::select! {
                biased;
                changed = metrics_rx.changed() => {
                    if changed.is_err() {
                        // OpenRaft has dropped its metrics sender — the engine is gone, which
                        // happens on graceful shutdown after `raft.shutdown()`.
                        tracing::info!("OpenRaft metrics channel closed; leadership monitor exiting");
                        break;
                    }
                }
                _ = probe_timer.tick() => {}
            }

            let metrics = metrics_rx.borrow().clone();
            let metrics_key = (metrics.state, metrics.current_term, metrics.current_leader);
            if last_metrics_key.as_ref() != Some(&metrics_key) {
                tracing::debug!(
                    "OpenRaft metrics changed: state={:?}, term={}, leader={:?}",
                    metrics.state,
                    metrics.current_term,
                    metrics.current_leader
                );
                last_metrics_key = Some(metrics_key);
            }

            let claims_leader = matches!(metrics.state, ServerState::Leader);
            if !claims_leader {
                // Once we stop claiming leader, any in-progress streak is moot; the role
                // change itself is logged below. Clearing `last_probe_at` ensures the next
                // false→true edge probes immediately rather than waiting out an interval.
                streak = None;
                leader_confirmed = false;
                last_probe_at = None;
            } else if !leader_confirmed
                && last_probe_at.is_none_or(|t| t.elapsed() >= PROBE_INTERVAL)
            {
                last_probe_at = Some(Instant::now());
                match timeout(PROBE_TIMEOUT, raft.ensure_linearizable()).await {
                    Ok(Ok(_)) => {
                        if let Some(s) = streak.take() {
                            tracing::info!(
                                "raft leader confirmed (recovered from {:?} after {:?})",
                                s.kind,
                                s.started_at.elapsed()
                            );
                        } else {
                            tracing::info!("raft leader confirmed");
                        }
                        leader_confirmed = true;
                    }
                    Ok(Err(err)) => {
                        note_failure(&mut streak, classify(&err), Some(&err));
                    }
                    Err(_) => {
                        note_failure(&mut streak, ProbeFailure::Timeout, None);
                    }
                }
            }

            let role = if claims_leader && leader_confirmed {
                ConsensusRole::Leader
            } else {
                ConsensusRole::Replica
            };
            if role != prev_role {
                tracing::info!("OpenRaft leadership status changed: {role:?}");
                let was_leader = prev_role == ConsensusRole::Leader;
                prev_role = role;
                // Losing leadership mid-flight leaves the produce pipeline in an unrecoverable
                // state (e.g. a `Produce` parked in `BlockExecutor` waiting on an empty
                // mempool). Tear the runtime down so the orchestrator restarts the node and
                // it rejoins as a follower with fresh raft state.
                if was_leader && role != ConsensusRole::Leader {
                    panic!("raft leadership lost; tearing down node");
                }
            }

            let status = RaftConsensusStatus {
                node_id: node_id_str.clone(),
                state: format!("{:?}", metrics.state),
                is_leader: role == ConsensusRole::Leader,
                current_leader: metrics.current_leader.map(|id| id.to_string()),
                current_term: metrics.current_term,
                last_applied_index: metrics.last_applied.map(|id| id.index),
            };
            // status_tx may have no receivers if the status server is disabled; that's fine.
            let _ = status_tx.send(Some(status));
            if leader_tx.send(role).is_err() {
                break;
            }
        }
    });
}

fn classify(err: &LinearizableErr) -> ProbeFailure {
    match err {
        RaftError::APIError(CheckIsLeaderError::ForwardToLeader(_)) => {
            ProbeFailure::ForwardToLeader
        }
        RaftError::APIError(CheckIsLeaderError::QuorumNotEnough(_)) => {
            ProbeFailure::QuorumNotEnough
        }
        RaftError::Fatal(_) => ProbeFailure::Fatal,
    }
}

fn note_failure(
    streak: &mut Option<FailureStreak>,
    kind: ProbeFailure,
    err: Option<&LinearizableErr>,
) {
    let now = Instant::now();
    match streak {
        Some(s) if s.kind == kind => {
            // Same failure as last tick: stay quiet unless the reminder window has elapsed.
            if now.duration_since(s.last_logged_at) >= STUCK_REMINDER_INTERVAL {
                emit_failure(kind, err, Some(now.duration_since(s.started_at)));
                s.last_logged_at = now;
            }
        }
        _ => {
            emit_failure(kind, err, None);
            *streak = Some(FailureStreak {
                kind,
                started_at: now,
                last_logged_at: now,
            });
        }
    }
}

fn emit_failure(kind: ProbeFailure, err: Option<&LinearizableErr>, elapsed: Option<Duration>) {
    let stuck = elapsed
        .map(|e| format!(" (still failing after {e:?})"))
        .unwrap_or_default();
    match kind {
        ProbeFailure::ForwardToLeader => {
            // Expected after a failover or for a stale leader catching up — surface as
            // INFO rather than WARN so it doesn't read like an alarm.
            let leader = err
                .and_then(|e| match e {
                    RaftError::APIError(CheckIsLeaderError::ForwardToLeader(f)) => f.leader_id,
                    _ => None,
                })
                .map(|id| format!("{id}"))
                .unwrap_or_else(|| "(unknown)".to_string());
            tracing::info!("raft node deposed: cluster leader is now {leader}{stuck}");
        }
        ProbeFailure::QuorumNotEnough => {
            // The operational alarm: this node holds the leader role but cannot reach a
            // quorum to commit, so the cluster cannot make progress. The `cluster` field
            // openraft attaches is a pre-formatted Debug dump of the full membership and
            // is too noisy to log; the acked set alone is enough to tell who replied.
            let acked = err
                .and_then(|e| match e {
                    RaftError::APIError(CheckIsLeaderError::QuorumNotEnough(q)) => Some(format!(
                        ", acked by {} of cluster: {:?}",
                        q.got.len(),
                        q.got
                    )),
                    _ => None,
                })
                .unwrap_or_default();
            tracing::warn!("raft cannot reach quorum{acked}{stuck}");
        }
        ProbeFailure::Timeout => {
            tracing::warn!("raft quorum probe timed out after {PROBE_TIMEOUT:?}{stuck}");
        }
        ProbeFailure::Fatal => {
            tracing::error!("raft is in a fatal state{stuck}");
        }
    }
}
