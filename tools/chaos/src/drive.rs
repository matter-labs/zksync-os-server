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
    /// How long L1 settlement (committed/executed batch counters, read from L1
    /// itself) may stand still while the chain finalizes and the settler is
    /// believed healthy. Generous: batches seal on a timeout and provers take
    /// their time; the target is a *dead-but-running* settler, not jitter.
    #[arg(long, default_value = "120s")]
    pub settlement_lag_window: humantime::Duration,
    /// Disable the L1 fault lane (anvil blackouts and base-fee spikes).
    #[arg(long)]
    pub no_l1_faults: bool,
    /// Enable shallow L1 reorgs (anvil_reorg) in the L1 fault lane. Off by
    /// default: a probe for l1_watcher assumptions, not a standing fault —
    /// the node may simply not be ready for reorgs yet.
    #[arg(long)]
    pub l1_reorgs: bool,
    /// Fault-mix profile. `default`: the full menu under the quorum constraint,
    /// with rare sanctioned outages. `gentle`: at most one fault outstanding
    /// and no sanctioned outages — the profile for committee-rotation bands,
    /// where a sanctioned outage could let the chain progress legitimately
    /// while the driver believes quorum is gone (a false safety finding).
    /// `restart-storm`: kills and graceful stops with the shortest heal
    /// horizons — process-lifecycle pressure (shutdown races, restart replay).
    #[arg(long, value_enum, default_value_t = Profile::Default)]
    pub profile: Profile,
    /// Per-poll cluster metrics (JSONL: elapsed, expectations, every node's
    /// finalized round / applied height, settlement counters). Defaults to
    /// `metrics.jsonl` in the work directory. Post-run analysis derives block
    /// rates, finality latency, and idle cadence from it.
    #[arg(long)]
    pub metrics: Option<PathBuf>,
    /// Restore freezing on SettlementStall findings. Off by default while the
    /// known prover-job strand — a leaked prover-job assignment that freezes
    /// the execute train until the job times out, chain-safe — is unfixed on
    /// the node side: stalls are logged to stdout and the metrics stream
    /// instead of freezing the run. Turn this on once the node fix lands.
    #[arg(long)]
    pub fail_on_settlement_stall: bool,
}

/// Which fault mix the schedule draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
pub enum Profile {
    Default,
    Gentle,
    RestartStorm,
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
    /// Lossy, slow network (tc netem) — degraded but alive: it still counts toward
    /// liveness expectations, because consensus is supposed to ride through it.
    Degraded,
}

/// A network-degradation profile applied with `tc netem`: packet loss plus delay
/// with jitter. Profiles are deliberately conservative — bad-but-survivable
/// weather; a committee that cannot stay live through them is a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct NetemProfile {
    pub loss_percent: u8,
    pub delay_ms: u16,
}

impl NetemProfile {
    /// The menu the schedule samples from.
    const MENU: [NetemProfile; 3] = [
        NetemProfile {
            loss_percent: 5,
            delay_ms: 50,
        },
        NetemProfile {
            loss_percent: 15,
            delay_ms: 150,
        },
        NetemProfile {
            loss_percent: 30,
            delay_ms: 300,
        },
    ];

    pub fn jitter_ms(&self) -> u16 {
        self.delay_ms / 3
    }
}

/// What the driver can do to the cluster. The I/O layer maps these onto docker
/// commands (and, for the L1 lane, anvil RPC calls); tests record them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "action", content = "target")]
pub enum Action {
    Kill(usize),
    Stop(usize),
    Start(usize),
    Pause(usize),
    Unpause(usize),
    Disconnect(usize),
    Reconnect(usize),
    Degrade(usize, NetemProfile),
    ClearDegradation(usize),
    /// Pause the anvil container: the L1 goes dark. L2 consensus is expected
    /// to keep finalizing right through it — that is the assertion — while
    /// L1-facing components (watcher, batcher senders) must cope and recover.
    L1Blackout,
    L1Restore,
    /// Multiply the L1 base fee for the next blocks (EIP-1559 decay on anvil's
    /// mostly-empty blocks walks it back down — no explicit heal needed).
    L1FeeSpike(u32),
    /// Replace the last N unfinalized L1 blocks (`anvil_reorg`). Behind
    /// `--l1-reorgs`, findings-only.
    L1Reorg(u64),
}

/// The pure decision core: owns the seeded RNG and the believed cluster state, and
/// produces one action at a time. Every fault it schedules comes with its own healing
/// (restarts after kills/stops, unpauses, reconnects) after a bounded number of steps.
pub struct Schedule {
    rng: StdRng,
    conditions: Vec<Condition>,
    quorum: usize,
    profile: Profile,
    /// Heals due at a given step: (due_step, action).
    pending_heals: Vec<(u64, Action)>,
    step: u64,
    /// The L1 fault lane; off unless enabled (tests of the validator lane, and
    /// `--no-l1-faults`, run without it).
    l1_faults: bool,
    l1_reorgs: bool,
    l1_blackout: bool,
}

impl Schedule {
    pub fn new(seed: u64, validators: usize, quorum: usize) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
            conditions: vec![Condition::Healthy; validators],
            quorum,
            profile: Profile::Default,
            pending_heals: Vec::new(),
            step: 0,
            l1_faults: false,
            l1_reorgs: false,
            l1_blackout: false,
        }
    }

    pub fn with_profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    pub fn with_l1_faults(mut self, reorgs: bool) -> Self {
        self.l1_faults = true;
        self.l1_reorgs = reorgs;
        self
    }

    pub fn l1_blackout(&self) -> bool {
        self.l1_blackout
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

    /// Validators expected to participate in consensus right now: pristine ones
    /// plus degraded ones — a lossy, slow network is exactly the weather consensus
    /// must ride through, so degradation never excuses a stall.
    pub fn live_count(&self) -> usize {
        self.conditions
            .iter()
            .filter(|&&condition| matches!(condition, Condition::Healthy | Condition::Degraded))
            .count()
    }

    /// Whether the committee is expected to finalize right now — the fact a monitor
    /// needs to decide if a stalled chain is a finding or the driver's own doing.
    pub fn expect_liveness(&self) -> bool {
        self.live_count() >= self.quorum
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

        // The L1 lane, when enabled: rarer than validator faults, and never
        // stacked (no fee business while the L1 is dark). L2 liveness stays
        // expected through all of it — an L1 blackout must not stall consensus,
        // and that is precisely what the watcher keeps checking.
        if self.l1_faults && !self.l1_blackout && self.rng.gen_ratio(1, 8) {
            let action = if self.l1_reorgs && self.rng.gen_ratio(1, 3) {
                Action::L1Reorg(self.rng.gen_range(3..=8))
            } else if self.rng.gen_ratio(1, 2) {
                let heal_in = self.rng.gen_range(2..=4);
                self.pending_heals
                    .push((self.step + heal_in, Action::L1Restore));
                Action::L1Blackout
            } else {
                Action::L1FeeSpike(self.rng.gen_range(5..=40))
            };
            self.apply(action);
            return action;
        }

        // A small chance to sanction a full outage: take any node down regardless of
        // quorum, with a short heal horizon. Everything else respects quorum —
        // counting degraded validators as live, since they are expected to keep
        // participating. Gentle never sanctions (see the flag docs) and holds
        // to one outstanding fault, so its liveness expectation stays exact
        // even when the active committee is a strict subset of the cluster.
        let sanction_outage = self.profile == Profile::Default && self.rng.gen_ratio(1, 20);
        let can_break_another = match self.profile {
            Profile::Gentle => self.healthy_count() == self.conditions.len(),
            Profile::Default | Profile::RestartStorm => {
                self.live_count() > self.quorum || sanction_outage
            }
        };

        let action = if can_break_another && self.rng.gen_ratio(2, 3) {
            // Break something.
            let target = self.pick_healthy();
            let heal_in = match self.profile {
                // Storms churn: the heal lands almost immediately, so the next
                // draw is free to break again — restart frequency is the point.
                Profile::RestartStorm => self.rng.gen_range(1..=2),
                Profile::Default | Profile::Gentle => self.rng.gen_range(2..=6),
            };
            let menu = match self.profile {
                // Kills twice as often as graceful stops: the dirty path is
                // where the shutdown-race class lives.
                Profile::RestartStorm => self.rng.gen_range(0..3u8).min(1),
                Profile::Default | Profile::Gentle => self.rng.gen_range(0..5u8),
            };
            let (fault, heal) = match menu {
                0 => (Action::Kill(target), Action::Start(target)),
                1 => (Action::Stop(target), Action::Start(target)),
                2 => (Action::Pause(target), Action::Unpause(target)),
                3 => (Action::Disconnect(target), Action::Reconnect(target)),
                _ => {
                    let profile =
                        NetemProfile::MENU[self.rng.gen_range(0..NetemProfile::MENU.len())];
                    (
                        Action::Degrade(target, profile),
                        Action::ClearDegradation(target),
                    )
                }
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
            Action::Degrade(index, _) => (index, Condition::Degraded),
            Action::Start(index)
            | Action::Unpause(index)
            | Action::Reconnect(index)
            | Action::ClearDegradation(index) => (index, Condition::Healthy),
            Action::L1Blackout => {
                self.l1_blackout = true;
                return;
            }
            Action::L1Restore => {
                self.l1_blackout = false;
                return;
            }
            Action::L1FeeSpike(_) | Action::L1Reorg(_) => return,
        };
        self.conditions[index] = condition;
    }

    /// The heals that would restore full health — applied on shutdown.
    pub fn heal_everything(&self) -> Vec<Action> {
        let mut heals: Vec<Action> = self
            .conditions
            .iter()
            .enumerate()
            .filter_map(|(index, condition)| match condition {
                Condition::Healthy => None,
                Condition::Killed | Condition::Stopped => Some(Action::Start(index)),
                Condition::Paused => Some(Action::Unpause(index)),
                Condition::Partitioned => Some(Action::Reconnect(index)),
                Condition::Degraded => Some(Action::ClearDegradation(index)),
            })
            .collect();
        if self.l1_blackout {
            heals.push(Action::L1Restore);
        }
        heals
    }
}

/// Applies actions to a real cluster. One implementation shells out to docker; tests
/// record actions instead.
pub trait ClusterOps {
    async fn apply(&mut self, action: Action) -> anyhow::Result<()>;
}

/// Docker-backed operations, addressing containers by the names `chaos setup`
/// wrote; L1-lane actions talk to anvil's host-published RPC.
pub struct DockerOps {
    manifest: Manifest,
    l1_rpc: reqwest::Client,
}

/// The anvil container `chaos setup` writes into the compose file.
const ANVIL_CONTAINER: &str = "chaos-anvil";

/// Docker CLI calls are asynchronous and hard-bounded so a slow daemon degrades
/// one injection, never the driver's clock (stop waits for the container's grace
/// period, hence the generous bound).
const DOCKER_TIMEOUT: Duration = Duration::from_secs(30);

impl DockerOps {
    fn new(manifest: Manifest) -> DockerOps {
        DockerOps {
            manifest,
            l1_rpc: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("default reqwest client"),
        }
    }

    fn container(&self, index: usize) -> String {
        format!("chaos-{}", self.manifest.validators[index].name)
    }

    async fn anvil(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let response: serde_json::Value = self
            .l1_rpc
            .post(format!("http://127.0.0.1:{}", self.manifest.host_l1_port))
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }))
            .send()
            .await?
            .json()
            .await?;
        anyhow::ensure!(
            response["error"].is_null(),
            "{method} failed: {}",
            response["error"],
        );
        Ok(response["result"].clone())
    }

    async fn docker(&self, args: &[&str]) -> anyhow::Result<()> {
        let command = tokio::process::Command::new("docker").args(args).output();
        let output = tokio::time::timeout(DOCKER_TIMEOUT, command)
            .await
            .map_err(|_| anyhow::anyhow!("docker {args:?} timed out after {DOCKER_TIMEOUT:?}"))??;
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
    async fn apply(&mut self, action: Action) -> anyhow::Result<()> {
        let network = self.manifest.network.clone();
        match action {
            Action::Kill(index) => {
                self.docker(&["kill", "--signal", "SIGKILL", &self.container(index)])
                    .await
            }
            Action::Stop(index) => self.docker(&["stop", &self.container(index)]).await,
            Action::Start(index) => self.docker(&["start", &self.container(index)]).await,
            Action::Pause(index) => self.docker(&["pause", &self.container(index)]).await,
            Action::Unpause(index) => self.docker(&["unpause", &self.container(index)]).await,
            Action::Disconnect(index) => {
                self.docker(&["network", "disconnect", &network, &self.container(index)])
                    .await
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
                .await
            }
            Action::Degrade(index, profile) => {
                // `replace` is idempotent: re-degrading swaps the profile in place.
                // `-u 0`: tc needs root inside the container (the node itself runs
                // unprivileged); the compose file grants NET_ADMIN.
                self.docker(&[
                    "exec",
                    "-u",
                    "0",
                    &self.container(index),
                    "tc",
                    "qdisc",
                    "replace",
                    "dev",
                    "eth0",
                    "root",
                    "netem",
                    "loss",
                    &format!("{}%", profile.loss_percent),
                    "delay",
                    &format!("{}ms", profile.delay_ms),
                    &format!("{}ms", profile.jitter_ms()),
                ])
                .await
            }
            Action::ClearDegradation(index) => {
                self.docker(&[
                    "exec",
                    "-u",
                    "0",
                    &self.container(index),
                    "tc",
                    "qdisc",
                    "del",
                    "dev",
                    "eth0",
                    "root",
                ])
                .await
            }
            Action::L1Blackout => self.docker(&["pause", ANVIL_CONTAINER]).await,
            Action::L1Restore => self.docker(&["unpause", ANVIL_CONTAINER]).await,
            Action::L1FeeSpike(multiplier) => {
                let block = self
                    .anvil("eth_getBlockByNumber", serde_json::json!(["latest", false]))
                    .await?;
                let base = u128::from_str_radix(
                    block["baseFeePerGas"]
                        .as_str()
                        .unwrap_or("0x1")
                        .trim_start_matches("0x"),
                    16,
                )?
                .max(1_000_000_000);
                let spiked = base.saturating_mul(u128::from(multiplier));
                self.anvil(
                    "anvil_setNextBlockBaseFeePerGas",
                    serde_json::json!([format!("0x{spiked:x}")]),
                )
                .await?;
                Ok(())
            }
            Action::L1Reorg(depth) => {
                // Replace the last `depth` unfinalized blocks with fresh ones.
                self.anvil("anvil_reorg", serde_json::json!([depth, []]))
                    .await?;
                Ok(())
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
    l1_blackout: bool,
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

    let mut schedule = Schedule::new(args.seed, validators, quorum).with_profile(args.profile);
    if !args.no_l1_faults {
        schedule = schedule.with_l1_faults(args.l1_reorgs);
    }
    let metrics_path = args
        .metrics
        .clone()
        .unwrap_or_else(|| args.workdir.join("metrics.jsonl"));
    let metrics = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&metrics_path)?;
    let probes = watch::NodeProbe::from_manifest(&manifest);
    // Settlement progress is read from L1 itself, so the watcher's view of the
    // settler outlives the settler. Old manifests predate the diamond field —
    // they run without the settlement check.
    let settlement = (!manifest.diamond_address.is_empty()).then(|| watch::SettlementWatch {
        probe: watch::SettlementProbe {
            l1_url: format!("http://127.0.0.1:{}", manifest.host_l1_port),
            diamond: manifest.diamond_address.clone(),
        },
        settler: manifest.settler,
        lag_window: *args.settlement_lag_window,
    });
    let mut ops = DockerOps::new(manifest);
    let started = std::time::Instant::now();
    let deadline = args
        .duration
        .map(|duration| tokio::time::Instant::now() + *duration);

    // The watcher learns the driver's beliefs through a watch channel and reports
    // the first finding back once; the driver then freezes the experiment.
    let (expectations_sender, expectations_receiver) =
        tokio::sync::watch::channel(watch::Expectations {
            conditions: schedule.conditions(),
            expect_liveness: schedule.expect_liveness(),
            since: started,
            l1_blackout: schedule.l1_blackout(),
        });
    let (findings_sender, mut findings_receiver) = tokio::sync::mpsc::channel(1);
    let watcher = tokio::spawn(watch::watch(
        probes,
        settlement,
        expectations_receiver,
        findings_sender,
        watch::WatchOptions {
            settle_margin: *args.settle_margin,
            liveness_window: *args.liveness_window,
            metrics: Some(metrics),
            fail_on_settlement_stall: args.fail_on_settlement_stall,
        },
    ));
    let mut previous_liveness = schedule.expect_liveness();
    let mut liveness_since = started;

    println!(
        "driving {validators} validators (quorum {quorum}) with seed {} and profile {:?}; \
         journal at {}, metrics at {}",
        args.seed,
        args.profile,
        journal_path.display(),
        metrics_path.display(),
    );

    loop {
        // Sample the gap before each fault around the configured interval (½x..1½x).
        let base = args.fault_interval.as_millis() as u64;
        let gap = Duration::from_millis(schedule.rng.gen_range(base / 2..=base * 3 / 2));
        tokio::select! {
            _ = tokio::time::sleep(gap) => {}
            // The deadline is its own select arm: a long fault gap must not delay
            // ending the run (the check used to only happen after a completed gap).
            _ = async { tokio::time::sleep_until(deadline.unwrap()).await },
                if deadline.is_some() => break,
            _ = tokio::signal::ctrl_c() => break,
            received = findings_receiver.recv() => {
                return freeze(&args.workdir, &journal_path, &ops, received);
            }
        }

        let action = schedule.next_action();
        println!(
            "[{:>6}s] {:?} (live {}/{}, liveness expected: {})",
            started.elapsed().as_secs(),
            action,
            schedule.live_count(),
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
            Action::Start(_)
                | Action::Unpause(_)
                | Action::Reconnect(_)
                | Action::ClearDegradation(_)
                | Action::L1Restore
        );
        if !heals {
            publish_expectations(
                &expectations_sender,
                &schedule,
                &mut previous_liveness,
                &mut liveness_since,
            );
        }
        if let Err(error) = ops.apply(action).await {
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
            l1_blackout: schedule.l1_blackout(),
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
        if let Err(error) = ops.apply(heal).await {
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

/// `chaos heal`: restore a cluster the driver left frozen (or otherwise wounded)
/// to full health, tolerantly — an orchestrator's resume step after a finding
/// froze the scene and its artifacts were collected. From the outside the
/// containers' exact conditions are unknown, so every restorative action is
/// applied to every node and failures ("already running", "no qdisc", "already
/// connected") are informational.
#[derive(Args)]
pub struct HealArgs {
    /// Work directory produced by `chaos setup`.
    #[arg(long)]
    pub workdir: PathBuf,
}

pub async fn heal(args: HealArgs) -> anyhow::Result<()> {
    let manifest: Manifest = serde_json::from_str(&std::fs::read_to_string(
        args.workdir.join("manifest.json"),
    )?)?;
    let count = manifest.validators.len();
    let mut ops = DockerOps::new(manifest);
    // Unpause first (a paused container ignores start), then start, then clear
    // the network-level faults (which need the container running).
    let mut restored = 0usize;
    for index in 0..count {
        for action in [
            Action::Unpause(index),
            Action::Start(index),
            Action::ClearDegradation(index),
            Action::Reconnect(index),
        ] {
            match ops.apply(action).await {
                Ok(()) => restored += 1,
                Err(error) => println!("  {action:?}: {error} (fine if already healthy)"),
            }
        }
    }
    if let Err(error) = ops.apply(Action::L1Restore).await {
        println!("  L1Restore: {error} (fine if the L1 was never paused)");
    }
    println!("heal pass complete ({restored} actions took effect)");
    Ok(())
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
        l1_blackout: schedule.l1_blackout(),
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
    fn the_l1_lane_is_bounded_and_off_by_default() {
        // Off by default: the validator-lane tests above stay meaningful.
        assert!(
            !actions(42, 5, 4, 2000)
                .iter()
                .any(|action| matches!(action, Action::L1Blackout | Action::L1FeeSpike(_))),
        );

        // On: blackouts never stack, always heal, and never excuse a stall —
        // liveness expectation tracks validators only.
        let mut schedule = Schedule::new(7, 5, 4).with_l1_faults(false);
        let mut dark = false;
        let mut saw_blackout = false;
        for _ in 0..5000 {
            let action = schedule.next_action();
            match action {
                Action::L1Blackout => {
                    assert!(!dark, "blackout while already dark");
                    dark = true;
                    saw_blackout = true;
                }
                Action::L1Restore => dark = false,
                Action::L1Reorg(_) => panic!("reorg action while reorgs are disabled"),
                _ => {}
            }
            assert_eq!(schedule.l1_blackout(), dark);
        }
        assert!(saw_blackout, "the lane never fired in 5000 steps");

        // Reorgs appear only when asked for, with shallow depths.
        let mut schedule = Schedule::new(7, 5, 4).with_l1_faults(true);
        let mut saw_reorg = false;
        for _ in 0..5000 {
            if let Action::L1Reorg(depth) = schedule.next_action() {
                assert!((3..=8).contains(&depth));
                saw_reorg = true;
            }
        }
        assert!(saw_reorg, "reorgs enabled but never drawn in 5000 steps");
    }

    #[test]
    fn heal_everything_restores_a_dark_l1() {
        let mut schedule = Schedule::new(7, 5, 4).with_l1_faults(false);
        while !schedule.l1_blackout() {
            schedule.next_action();
        }
        assert!(schedule.heal_everything().contains(&Action::L1Restore));
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
    fn degradations_are_paired_with_clears_and_count_as_live() {
        let mut schedule = Schedule::new(11, 5, 4);
        let mut saw_degrade = false;
        for _ in 0..2_000 {
            let action = schedule.next_action();
            if let Action::Degrade(index, profile) = action {
                saw_degrade = true;
                assert!(profile.loss_percent <= 30, "profiles stay conservative");
                assert_eq!(schedule.conditions()[index], Condition::Degraded);
            }
            // Degraded validators keep counting toward liveness expectations.
            assert!(schedule.live_count() >= schedule.healthy_count());
        }
        assert!(saw_degrade, "the fault menu must produce degradations");
        for heal in schedule.heal_everything() {
            schedule.apply(heal);
        }
        assert_eq!(schedule.healthy_count(), 5);
    }

    #[test]
    fn gentle_holds_to_one_outstanding_fault_and_never_breaks_quorum() {
        // The rotation-band profile: its watcher soundness argument (no false
        // ProgressWithoutQuorum during shrunken-committee epochs) rests on
        // liveness staying expected throughout — pin exactly that.
        for seed in 0..50 {
            let mut schedule = Schedule::new(seed, 5, 4).with_profile(Profile::Gentle);
            for _ in 0..2_000 {
                schedule.next_action();
                assert!(
                    schedule.healthy_count() >= 4,
                    "seed {seed}: gentle broke more than one validator at once",
                );
                assert!(
                    schedule.expect_liveness(),
                    "seed {seed}: gentle lost the liveness expectation",
                );
            }
        }
    }

    #[test]
    fn restart_storm_draws_only_process_lifecycle_faults() {
        let mut schedule = Schedule::new(9, 5, 4).with_profile(Profile::RestartStorm);
        let (mut kills, mut stops) = (0u32, 0u32);
        for _ in 0..2_000 {
            match schedule.next_action() {
                Action::Kill(_) => kills += 1,
                Action::Stop(_) => stops += 1,
                Action::Start(_) => {}
                other => panic!("storm drew a non-lifecycle action: {other:?}"),
            }
        }
        assert!(
            kills > stops,
            "kills are the storm's bias ({kills} vs {stops})"
        );
        assert!(stops > 0, "graceful stops stay in the mix");
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
