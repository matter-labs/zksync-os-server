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
}

/// One validator, as observed during a poll.
#[derive(Debug, Clone, Serialize)]
pub struct NodeObservation {
    pub running: bool,
    pub paused: bool,
    /// `None` when unreachable (which is fine for stopped/paused/partitioned nodes).
    /// `(epoch, view)`: views restart from zero at every epoch boundary, so only the
    /// pair is monotone — comparing bare views across a boundary reads a healthy
    /// handoff as a finality regression.
    pub finalized_round: Option<(u64, u64)>,
    pub applied_height: Option<u64>,
    pub block_hash_at_probe: Option<String>,
    /// Sum of the consensus fault-evidence counters.
    pub evidence: u64,
    /// Log lines since the previous poll that match the forbidden patterns.
    pub suspicious_log_lines: Vec<String>,
}

/// The height whose hash every reachable validator was asked about this poll.
#[derive(Debug, Clone, Serialize)]
pub struct Poll {
    pub probe_height: Option<u64>,
    pub nodes: Vec<NodeObservation>,
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
    /// The committee was expected live for the whole window but no new finalization
    /// appeared.
    LivenessStall {
        window: Duration,
        at_round: (u64, u64),
    },
}

/// The pure check engine: state carried between polls, no I/O. The shell feeds it
/// observations; it returns findings.
pub struct Checker {
    /// Highest finalized `(epoch, view)` / applied height ever seen per validator.
    finalized_rounds: Vec<(u64, u64)>,
    applied_heights: Vec<u64>,
    evidence: Vec<u64>,
    /// The driver's beliefs at the previous poll, for spotting heal transitions.
    previous_conditions: Vec<Condition>,
    /// Highest finalized `(epoch, view)` seen anywhere, and when it last advanced.
    tip_round: (u64, u64),
    tip_advanced_at: Instant,
    /// The finalized-round baseline captured when liveness expectation was lost, once
    /// the settle margin passed. `None` while liveness is expected.
    forbidden_baseline: Option<(u64, u64)>,
    settle_margin: Duration,
    liveness_window: Duration,
}

impl Checker {
    pub fn new(validators: usize, settle_margin: Duration, liveness_window: Duration) -> Self {
        Self {
            finalized_rounds: vec![(0, 0); validators],
            applied_heights: vec![0; validators],
            evidence: vec![0; validators],
            previous_conditions: vec![Condition::Healthy; validators],
            tip_round: (0, 0),
            tip_advanced_at: Instant::now(),
            forbidden_baseline: None,
            settle_margin,
            liveness_window,
        }
    }

    pub fn observe(
        &mut self,
        now: Instant,
        expectations: &Expectations,
        poll: &Poll,
    ) -> Vec<Finding> {
        let mut findings = Vec::new();

        // A node healed from Killed/Stopped has *restarted*: its applied height is
        // the applier's per-run progress and legitimately starts over from the
        // restart replay range, so its baseline resets. Unpause and reconnect do not
        // restart the process, and finalized views come from the consensus journal
        // and stay monotone across restarts — neither resets.
        for (index, condition) in expectations.conditions.iter().enumerate() {
            if *condition == Condition::Healthy
                && matches!(
                    self.previous_conditions[index],
                    Condition::Killed | Condition::Stopped
                )
            {
                self.applied_heights[index] = 0;
            }
        }
        self.previous_conditions = expectations.conditions.clone();

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

            // "Not running" is only ever expected while the driver holds a node
            // killed or stopped; a paused, partitioned, degraded, or healthy
            // validator that is not running died on its own.
            if !matches!(
                expectations.conditions[index],
                Condition::Killed | Condition::Stopped
            ) && !node.running
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
            });
            // Re-arm so a genuinely stuck cluster produces one finding per window,
            // not one per poll.
            self.tip_advanced_at = now;
        }

        findings
    }
}

/// Log lines that always warrant a finding.
const FORBIDDEN_LOG_PATTERNS: [&str; 2] = ["panicked at", " ERROR "];
/// Known-benign teardown noise of a node being stopped by the driver itself:
/// the pipeline's critical tasks report their neighbor channels closing as
/// errors, and the runtime-drop panic is a registered shutdown wart (tracked in
/// the shortcut register; harmless but not yet eliminated). Deliberately narrow —
/// a *novel* critical-task panic must still surface.
// Two long-standing allowances were REMOVED after the commonware-2026.5.0
// upgrade soaks showed them gone (they fired routinely on 2026.4.0):
// the runtime-drop teardown wart ("Cannot drop a runtime" +
// "runtime/blocking/shutdown.rs") and marshal's empty-archive destroy panic
// ("failed to destroy", BlobMissing on follower-shaped per-epoch caches;
// dormant issue draft in consensus_planning/upstream-issues.md). If either
// resurfaces, the watcher now freezes on it — deliberate: a workaround must
// not outlive its wart.
const ALLOWED_LOG_PATTERNS: [&str; 2] = [
    "pipeline segment failed",
    "failed to receive deregistration",
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
    async fn container_state(&self) -> (bool, bool) {
        let output = docker_output(
            &[
                "inspect",
                "--format",
                "{{.State.Running}} {{.State.Paused}}",
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
                (running, paused)
            }
            None => (false, false),
        }
    }

    /// New log lines since `since` that match the forbidden patterns. `--tail`
    /// keeps the scan bounded no matter how old the container is.
    async fn suspicious_logs(&self, since: Instant, now: Instant) -> Vec<String> {
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
    ) -> NodeObservation {
        let (running, paused) = self.container_state().await;

        // All probes run concurrently: an unreachable node costs one client
        // timeout per poll, not a sum of them.
        let (status, block_hash_at_probe, evidence, suspicious_log_lines) = tokio::join!(
            self.status(client),
            async {
                match probe_height {
                    Some(height) if running && !paused => self.block_hash(client, height).await,
                    _ => None,
                }
            },
            self.evidence_total(client),
            self.suspicious_logs(logs_since, now),
        );
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

        NodeObservation {
            running,
            paused,
            finalized_round,
            applied_height,
            block_hash_at_probe,
            evidence,
            suspicious_log_lines,
        }
    }

    async fn status(&self, client: &reqwest::Client) -> Option<StatusResponse> {
        match client.get(&self.status_url).send().await {
            Ok(response) => response.json().await.ok(),
            Err(_) => None,
        }
    }

    async fn block_hash(&self, client: &reqwest::Client, height: u64) -> Option<String> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_getBlockByNumber",
            "params": [format!("0x{height:x}"), false],
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
        response["result"]["hash"]
            .as_str()
            .map(|hash| hash.to_string())
    }

    /// Sum of the protocol fault-evidence counters from the node's metrics text.
    pub async fn evidence_total(&self, client: &reqwest::Client) -> u64 {
        let Ok(response) = client.get(&self.metrics_url).send().await else {
            return 0;
        };
        let Ok(text) = response.text().await else {
            return 0;
        };
        text.lines()
            .filter(|line| {
                line.starts_with("consensus_activity")
                    && (line.contains("conflicting_notarize")
                        || line.contains("conflicting_finalize")
                        || line.contains("nullify_finalize"))
            })
            .filter_map(|line| line.split_whitespace().last())
            .filter_map(|value| value.parse::<f64>().ok())
            .sum::<f64>() as u64
    }
}

/// How often the watcher polls the cluster.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The watcher loop: polls every validator, feeds the [`Checker`], and sends the
/// first non-empty batch of findings (with the poll that produced it) before
/// returning. Sending once and stopping is deliberate — the driver freezes the
/// experiment on the first finding, so there is nothing left to watch.
pub async fn watch(
    probes: Vec<NodeProbe>,
    expectations: tokio::sync::watch::Receiver<Expectations>,
    findings: tokio::sync::mpsc::Sender<(Poll, Vec<Finding>)>,
    settle_margin: Duration,
    liveness_window: Duration,
) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .expect("default reqwest client");
    let mut checker = Checker::new(probes.len(), settle_margin, liveness_window);
    // The height probed for agreement: the lowest applied height reported in the
    // *previous* poll, so every reachable validator has the block by now.
    let mut probe_height: Option<u64> = None;
    let mut logs_since = Instant::now();

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let now = Instant::now();
        let current_expectations = expectations.borrow().clone();

        let observations = futures::future::join_all(
            probes
                .iter()
                .map(|probe| probe.observe(&client, probe_height, logs_since, now)),
        )
        .await;
        logs_since = now;

        let poll = Poll {
            probe_height,
            nodes: observations,
        };
        probe_height = poll
            .nodes
            .iter()
            .filter_map(|node| node.applied_height)
            .min();

        let batch = checker.observe(now, &current_expectations, &poll);
        if !batch.is_empty() {
            let _ = findings.send((poll, batch)).await;
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
        }
    }

    /// An observation in epoch 0 — the view doubles as the applied height.
    fn observation(finalized_view: u64, hash: &str) -> NodeObservation {
        NodeObservation {
            running: true,
            paused: false,
            finalized_round: Some((0, finalized_view)),
            applied_height: Some(finalized_view),
            block_hash_at_probe: Some(hash.to_string()),
            evidence: 0,
            suspicious_log_lines: Vec::new(),
        }
    }

    #[test]
    fn agreement_violation_is_a_finding() {
        let mut checker = Checker::new(2, Duration::from_secs(5), Duration::from_secs(60));
        let poll = Poll {
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
        let mut checker = Checker::new(3, Duration::from_secs(5), Duration::from_secs(60));
        let now = Instant::now();
        let mut expectations = healthy_expectations(3);
        let poll = |view| Poll {
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
        let mut checker = Checker::new(2, Duration::from_secs(5), Duration::from_secs(60));
        let start = Instant::now();
        let expectations = healthy_expectations(2);
        let poll = Poll {
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

    #[test]
    fn regression_and_evidence_and_death_are_findings() {
        let mut checker = Checker::new(1, Duration::from_secs(5), Duration::from_secs(60));
        let expectations = healthy_expectations(1);
        let now = Instant::now();

        checker.observe(
            now,
            &expectations,
            &Poll {
                probe_height: None,
                nodes: vec![observation(10, "0x")],
            },
        );

        let mut regressed = observation(4, "0x");
        regressed.evidence = 1;
        regressed.running = false;
        let findings = checker.observe(
            now,
            &expectations,
            &Poll {
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
    fn view_restart_at_an_epoch_boundary_is_not_a_regression() {
        let mut checker = Checker::new(1, Duration::from_secs(5), Duration::from_secs(60));
        let expectations = healthy_expectations(1);
        let now = Instant::now();
        let poll = |epoch, view| Poll {
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
        let mut checker = Checker::new(1, Duration::from_secs(5), Duration::from_secs(60));
        let now = Instant::now();
        let poll = |applied| Poll {
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
                probe_height: None,
                nodes: vec![NodeObservation {
                    running: false,
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
    fn log_filter_allows_known_shutdown_noise() {
        assert!(is_suspicious_log_line(
            "thread 'main' panicked at lib/x.rs:1:1"
        ));
        assert!(is_suspicious_log_line("2026-07-04 ERROR something new"));
        assert!(!is_suspicious_log_line(
            "ERROR reth_tasks::runtime: Critical task `pipeline` panicked: `failed to receive deregistration`"
        ));
        // The registered teardown wart is tolerated — both of its lines: the
        // site-bearing first line and the message-bearing second.
        assert!(!is_suspicious_log_line(
            "thread 'tokio-rt-worker' (215) panicked at /usr/local/cargo/registry/\
             src/index.crates.io-xxx/tokio-1.52.3/src/runtime/blocking/shutdown.rs:51:21:"
        ));
        assert!(!is_suspicious_log_line(
            "Cannot drop a runtime in a context where blocking is not allowed"
        ));
        // ...but a *novel* critical-task panic is not.
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
