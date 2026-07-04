//! The seeded fault driver: repeatedly picks a fault from a small menu and applies it
//! to the cluster, under one invariant — **the driver always knows whether the
//! committee should be live**. It never reduces the healthy set below quorum except
//! through a deliberate, bounded outage window, and every action lands in a JSONL
//! journal alongside the expected cluster health — and is published to the in-process
//! watcher (see [`crate::watch`]) — so "the driver took quorum away" is always
//! distinguishable from "the chain stalled and nobody knows why". On the watcher's
//! first finding the driver freezes the experiment instead of healing it.
//!
//! Determinism honesty: the seed fully determines the *schedule* (what, whom, when in
//! elapsed terms); the system's response runs on real time and real machines, so a
//! seed replays the experiment, not the execution.
//!
//! The decision core ([`Schedule`]) is pure — same seed, same decisions — and unit
//! tests drive it over thousands of steps to check the constraints. The I/O shell
//! around it shells out to `docker`.

use crate::setup::Manifest;
use crate::watch;
use clap::Args;
use rand08::rngs::StdRng;
use rand08::{Rng, SeedableRng};
use serde::Serialize;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Args)]
pub struct DriveArgs {
    /// Work directory produced by `chaos setup` (with a running cluster).
    #[arg(long)]
    pub workdir: PathBuf,
    /// Seed for the fault schedule.
    #[arg(long)]
    pub seed: u64,
    /// How long to run; omit to run until interrupted.
    #[arg(long)]
    pub duration: Option<humantime::Duration>,
    /// Rough time between faults (each actual gap is sampled around this).
    #[arg(long, default_value = "30s")]
    pub fault_interval: humantime::Duration,
    /// Journal file (JSONL, one entry per action). Defaults to `journal.jsonl` in the
    /// work directory.
    #[arg(long)]
    pub journal: Option<PathBuf>,
    /// How long in-flight finalizations may still land after quorum is taken away
    /// before any further progress counts as a safety violation.
    #[arg(long, default_value = "5s")]
    pub settle_margin: humantime::Duration,
    /// How long the committee may go without a new finalization while the driver
    /// expects it to be live. Generous on purpose: a false alarm costs minutes.
    #[arg(long, default_value = "60s")]
    pub liveness_window: humantime::Duration,
}

/// One validator's condition as the driver believes it to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Condition {
    Healthy,
    /// SIGKILL'd — the dirty crash. Restarted after a bounded delay.
    Killed,
    /// Gracefully stopped. Restarted after a bounded delay.
    Stopped,
    /// Frozen (SIGSTOP semantics via the container freezer) — the "infinite GC pause".
    Paused,
    /// Detached from the cluster network; everything else about it keeps running.
    Partitioned,
}

/// What the driver can do to the cluster. The I/O layer maps these onto docker
/// commands; tests record them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "action", content = "validator")]
pub enum Action {
    Kill(usize),
    Stop(usize),
    Start(usize),
    Pause(usize),
    Unpause(usize),
    Disconnect(usize),
    Reconnect(usize),
}

/// The pure decision core: owns the seeded RNG and the believed cluster state, and
/// produces one action at a time. Every fault it schedules comes with its own healing
/// (restarts after kills/stops, unpauses, reconnects) after a bounded number of steps.
pub struct Schedule {
    rng: StdRng,
    conditions: Vec<Condition>,
    quorum: usize,
    /// Heals due at a given step: (due_step, action).
    pending_heals: Vec<(u64, Action)>,
    step: u64,
}

impl Schedule {
    pub fn new(seed: u64, validators: usize, quorum: usize) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
            conditions: vec![Condition::Healthy; validators],
            quorum,
            pending_heals: Vec::new(),
            step: 0,
        }
    }

    pub fn conditions(&self) -> Vec<Condition> {
        self.conditions.clone()
    }

    pub fn healthy_count(&self) -> usize {
        self.conditions
            .iter()
            .filter(|&&condition| condition == Condition::Healthy)
            .count()
    }

    /// Whether the committee is expected to finalize right now — the fact a monitor
    /// needs to decide if a stalled chain is a finding or the driver's own doing.
    pub fn expect_liveness(&self) -> bool {
        self.healthy_count() >= self.quorum
    }

    /// Produces the next action. Due heals always go first; otherwise a fault is
    /// drawn, constrained so the healthy set stays at or above quorum — except in a
    /// deliberately sanctioned outage (a rare draw), which is immediately followed by
    /// its heal on the next steps.
    pub fn next_action(&mut self) -> Action {
        self.step += 1;

        if let Some(position) = self
            .pending_heals
            .iter()
            .position(|(due, _)| *due <= self.step)
        {
            let (_, heal) = self.pending_heals.remove(position);
            self.apply(heal);
            return heal;
        }

        // A small chance to sanction a full outage: take any node down regardless of
        // quorum, with a short heal horizon. Everything else respects quorum.
        let sanction_outage = self.rng.gen_ratio(1, 20);
        let can_break_another = self.healthy_count() > self.quorum || sanction_outage;

        let action = if can_break_another && self.rng.gen_ratio(2, 3) {
            // Break something.
            let target = self.pick_healthy();
            let heal_in = self.rng.gen_range(2..=6);
            let (fault, heal) = match self.rng.gen_range(0..4u8) {
                0 => (Action::Kill(target), Action::Start(target)),
                1 => (Action::Stop(target), Action::Start(target)),
                2 => (Action::Pause(target), Action::Unpause(target)),
                _ => (Action::Disconnect(target), Action::Reconnect(target)),
            };
            self.pending_heals.push((self.step + heal_in, heal));
            fault
        } else {
            // Heal something early, or no-op into the nearest heal if all healthy.
            match self.pending_heals.pop() {
                Some((_, heal)) => heal,
                None => {
                    // Nothing broken and the dice said "don't break": kill-and-restart
                    // is the most valuable default exercise.
                    let target = self.pick_healthy();
                    self.pending_heals
                        .push((self.step + 1, Action::Start(target)));
                    Action::Kill(target)
                }
            }
        };
        self.apply(action);
        action
    }

    fn pick_healthy(&mut self) -> usize {
        let healthy: Vec<usize> = self
            .conditions
            .iter()
            .enumerate()
            .filter(|(_, condition)| **condition == Condition::Healthy)
            .map(|(index, _)| index)
            .collect();
        healthy[self.rng.gen_range(0..healthy.len())]
    }

    fn apply(&mut self, action: Action) {
        let (index, condition) = match action {
            Action::Kill(index) => (index, Condition::Killed),
            Action::Stop(index) => (index, Condition::Stopped),
            Action::Pause(index) => (index, Condition::Paused),
            Action::Disconnect(index) => (index, Condition::Partitioned),
            Action::Start(index) | Action::Unpause(index) | Action::Reconnect(index) => {
                (index, Condition::Healthy)
            }
        };
        self.conditions[index] = condition;
    }

    /// The heals that would restore full health — applied on shutdown.
    pub fn heal_everything(&self) -> Vec<Action> {
        self.conditions
            .iter()
            .enumerate()
            .filter_map(|(index, condition)| match condition {
                Condition::Healthy => None,
                Condition::Killed | Condition::Stopped => Some(Action::Start(index)),
                Condition::Paused => Some(Action::Unpause(index)),
                Condition::Partitioned => Some(Action::Reconnect(index)),
            })
            .collect()
    }
}

/// Applies actions to a real cluster. One implementation shells out to docker; tests
/// record actions instead.
pub trait ClusterOps {
    fn apply(&mut self, action: Action) -> anyhow::Result<()>;
}

/// Docker-backed operations, addressing containers by the names `chaos setup` wrote.
pub struct DockerOps {
    manifest: Manifest,
}

impl DockerOps {
    fn container(&self, index: usize) -> String {
        format!("chaos-{}", self.manifest.validators[index].name)
    }

    fn docker(&self, args: &[&str]) -> anyhow::Result<()> {
        let output = std::process::Command::new("docker").args(args).output()?;
        anyhow::ensure!(
            output.status.success(),
            "docker {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }
}

impl ClusterOps for DockerOps {
    fn apply(&mut self, action: Action) -> anyhow::Result<()> {
        let network = self.manifest.network.clone();
        match action {
            Action::Kill(index) => {
                self.docker(&["kill", "--signal", "SIGKILL", &self.container(index)])
            }
            Action::Stop(index) => self.docker(&["stop", &self.container(index)]),
            Action::Start(index) => self.docker(&["start", &self.container(index)]),
            Action::Pause(index) => self.docker(&["pause", &self.container(index)]),
            Action::Unpause(index) => self.docker(&["unpause", &self.container(index)]),
            Action::Disconnect(index) => {
                self.docker(&["network", "disconnect", &network, &self.container(index)])
            }
            Action::Reconnect(index) => {
                // Reattach at the pinned address: the committee dials this exact IP,
                // and a bare `connect` would draw a fresh dynamic one.
                let ip = self.manifest.validators[index].ip.clone();
                self.docker(&[
                    "network",
                    "connect",
                    "--ip",
                    &ip,
                    &network,
                    &self.container(index),
                ])
            }
        }
    }
}

/// One journal line: what was done, and what the monitor may expect afterwards.
#[derive(Serialize)]
struct JournalEntry {
    elapsed_ms: u128,
    seed: u64,
    step: u64,
    #[serde(flatten)]
    action: Action,
    healthy_after: usize,
    expect_liveness: bool,
}

pub async fn run(args: DriveArgs) -> anyhow::Result<()> {
    let manifest: Manifest = serde_json::from_str(&std::fs::read_to_string(
        args.workdir.join("manifest.json"),
    )?)?;
    let validators = manifest.validators.len();
    let quorum = manifest.quorum;
    let journal_path = args
        .journal
        .clone()
        .unwrap_or_else(|| args.workdir.join("journal.jsonl"));
    let mut journal = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&journal_path)?;

    let mut schedule = Schedule::new(args.seed, validators, quorum);
    let probes = watch::NodeProbe::from_manifest(&manifest);
    let mut ops = DockerOps { manifest };
    let started = std::time::Instant::now();
    let deadline = args.duration.map(|duration| started + *duration);

    // The watcher learns the driver's beliefs through a watch channel and reports
    // the first finding back once; the driver then freezes the experiment.
    let (expectations_sender, expectations_receiver) =
        tokio::sync::watch::channel(watch::Expectations {
            conditions: schedule.conditions(),
            expect_liveness: schedule.expect_liveness(),
            since: started,
        });
    let (findings_sender, mut findings_receiver) = tokio::sync::mpsc::channel(1);
    let watcher = tokio::spawn(watch::watch(
        probes,
        expectations_receiver,
        findings_sender,
        *args.settle_margin,
        *args.liveness_window,
    ));
    let mut previous_liveness = schedule.expect_liveness();
    let mut liveness_since = started;

    println!(
        "driving {validators} validators (quorum {quorum}) with seed {}; journal at {}",
        args.seed,
        journal_path.display(),
    );

    loop {
        // Sample the gap before each fault around the configured interval (½x..1½x).
        let base = args.fault_interval.as_millis() as u64;
        let gap = Duration::from_millis(schedule.rng.gen_range(base / 2..=base * 3 / 2));
        tokio::select! {
            _ = tokio::time::sleep(gap) => {}
            _ = tokio::signal::ctrl_c() => break,
            received = findings_receiver.recv() => {
                return freeze(&args.workdir, &journal_path, &ops, received);
            }
        }
        if let Some(deadline) = deadline
            && std::time::Instant::now() >= deadline
        {
            break;
        }

        let action = schedule.next_action();
        println!(
            "[{:>6}s] {:?} (healthy {}/{}, liveness expected: {})",
            started.elapsed().as_secs(),
            action,
            schedule.healthy_count(),
            validators,
            schedule.expect_liveness(),
        );

        // Ordering rule against watcher false positives: the watcher must never
        // observe a fault it does not yet expect (a killed node would look like an
        // unexpected death), and must never expect a heal that has not yet landed
        // (a node still starting would look dead). So faults publish expectations
        // first and then land; heals land first and then publish.
        let heals = matches!(
            action,
            Action::Start(_) | Action::Unpause(_) | Action::Reconnect(_)
        );
        if !heals {
            publish_expectations(
                &expectations_sender,
                &schedule,
                &mut previous_liveness,
                &mut liveness_since,
            );
        }
        if let Err(error) = ops.apply(action) {
            // A failed injection (e.g. the container is already in that state) is a
            // journalable event, not a reason to stop the experiment.
            println!("  action failed (continuing): {error}");
        }
        if heals {
            publish_expectations(
                &expectations_sender,
                &schedule,
                &mut previous_liveness,
                &mut liveness_since,
            );
        }

        let entry = JournalEntry {
            elapsed_ms: started.elapsed().as_millis(),
            seed: args.seed,
            step: schedule.step,
            action,
            healthy_after: schedule.healthy_count(),
            expect_liveness: schedule.expect_liveness(),
        };
        writeln!(journal, "{}", serde_json::to_string(&entry)?)?;
    }

    // A finding that raced Ctrl-C or the deadline still freezes the scene.
    if let Ok(received) = findings_receiver.try_recv() {
        return freeze(&args.workdir, &journal_path, &ops, Some(received));
    }

    // Clean exit: stop watching and leave the cluster whole.
    watcher.abort();
    println!("healing the cluster before exit");
    for heal in schedule.heal_everything() {
        if let Err(error) = ops.apply(heal) {
            println!("  heal failed: {error}");
        }
    }
    Ok(())
}

/// Handles the watcher's report: freeze the scene. Nothing is healed and the
/// process exits nonzero — the cluster stays up exactly as it failed, with the
/// findings, the offending poll, and every container's recent logs captured under
/// `<workdir>/artifacts/`.
fn freeze(
    workdir: &Path,
    journal_path: &Path,
    ops: &DockerOps,
    received: Option<(watch::Poll, Vec<watch::Finding>)>,
) -> anyhow::Result<()> {
    let Some((poll, findings)) = received else {
        anyhow::bail!("the watcher stopped unexpectedly; the experiment is unwatched");
    };
    for finding in &findings {
        println!("FINDING: {finding:?}");
    }

    let artifacts = workdir.join("artifacts");
    std::fs::create_dir_all(&artifacts)?;
    std::fs::write(
        artifacts.join("findings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "findings": findings,
            "poll": poll,
        }))?,
    )?;
    for validator in &ops.manifest.validators {
        let container = format!("chaos-{}", validator.name);
        let output = std::process::Command::new("docker")
            .args(["logs", "--tail", "10000", &container])
            .output();
        if let Ok(output) = output {
            let mut log = output.stdout;
            log.extend_from_slice(&output.stderr);
            std::fs::write(artifacts.join(format!("{}.log", validator.name)), log)?;
        }
    }
    anyhow::bail!(
        "finding(s) recorded at {}; the cluster is left frozen (not healed) for investigation — journal at {}",
        artifacts.display(),
        journal_path.display(),
    )
}

/// Publishes the driver's current beliefs to the watcher, timestamping the moment
/// the liveness expectation last flipped (the settle margin and the liveness
/// window both count from that moment).
fn publish_expectations(
    sender: &tokio::sync::watch::Sender<watch::Expectations>,
    schedule: &Schedule,
    previous_liveness: &mut bool,
    liveness_since: &mut std::time::Instant,
) {
    let expect_liveness = schedule.expect_liveness();
    if expect_liveness != *previous_liveness {
        *previous_liveness = expect_liveness;
        *liveness_since = std::time::Instant::now();
    }
    let _ = sender.send(watch::Expectations {
        conditions: schedule.conditions(),
        expect_liveness,
        since: *liveness_since,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays a schedule and returns every action taken.
    fn actions(seed: u64, validators: usize, quorum: usize, steps: usize) -> Vec<Action> {
        let mut schedule = Schedule::new(seed, validators, quorum);
        (0..steps).map(|_| schedule.next_action()).collect()
    }

    #[test]
    fn same_seed_same_schedule() {
        assert_eq!(actions(42, 5, 4, 500), actions(42, 5, 4, 500));
        assert_ne!(actions(42, 5, 4, 500), actions(43, 5, 4, 500));
    }

    #[test]
    fn outages_are_sanctioned_and_bounded() {
        // Below-quorum health must only ever follow a deliberate sanction, and must
        // heal within the bounded horizon (no permanent outage).
        for seed in 0..50 {
            let mut schedule = Schedule::new(seed, 5, 4);
            let mut below_quorum_streak = 0u32;
            for _ in 0..2_000 {
                schedule.next_action();
                if schedule.expect_liveness() {
                    below_quorum_streak = 0;
                } else {
                    below_quorum_streak += 1;
                    assert!(
                        below_quorum_streak <= 12,
                        "seed {seed}: outage lasted longer than its bounded heal horizon",
                    );
                }
            }
        }
    }

    #[test]
    fn healing_everything_restores_full_health() {
        for seed in 0..50 {
            let mut schedule = Schedule::new(seed, 7, 5);
            for _ in 0..500 {
                schedule.next_action();
            }
            for heal in schedule.heal_everything() {
                schedule.apply(heal);
            }
            assert_eq!(schedule.healthy_count(), 7, "seed {seed}");
        }
    }

    #[test]
    fn faults_spread_over_the_committee() {
        // Not a fairness proof — just a guard against the picker degenerating to one
        // victim.
        let actions = actions(7, 5, 4, 1_000);
        let mut touched = std::collections::BTreeSet::new();
        for action in actions {
            if let Action::Kill(index) | Action::Stop(index) | Action::Pause(index) = action {
                touched.insert(index);
            }
        }
        assert_eq!(touched.len(), 5, "every validator gets its turn");
    }
}
