//! The chaos rig: a containerized BFT validator cluster plus a seeded fault driver.
//!
//! `chaos setup` generates everything a `docker compose` cluster needs — committee
//! keys, per-validator node configs, the compose file, and a manifest the driver
//! reads. `chaos drive` then injects faults (kill, graceful stop, pause, network
//! partition) on a seeded random schedule, indefinitely or for a fixed duration,
//! journaling every action it takes.
//!
//! Two rules keep the randomness honest:
//!
//! - The **schedule** is fully determined by the seed. The *system's response* is not
//!   (real time, real disks, real sockets), so a seed replays the experiment, not the
//!   execution — artifacts captured on a violation matter more than the seed.
//! - The driver always knows what it broke: it never reduces the healthy set below
//!   quorum except through a deliberate, bounded "outage window" action, so an
//!   external monitor can tell "chain paused because the driver took quorum away"
//!   from "chain paused and nobody knows why".
//!
//! This tool is a telescope, not a gate: nothing in CI depends on it. Findings get
//! distilled into deterministic regression tests where they belong.

mod drive;
mod load;
mod promote;
mod setup;
mod watch;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "chaos", about = "BFT consensus chaos rig")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a work directory with keys, configs, compose file, and manifest.
    Setup(setup::SetupArgs),
    /// Run the seeded fault schedule against a running cluster.
    Drive(drive::DriveArgs),
    /// Promote observers into the committee, live (config rewrite + rolling
    /// restarts + manifest update). Run faults/load after it exits.
    Promote(promote::PromoteArgs),
    /// Put transaction load on a running cluster (fund + spam + report).
    Load(load::LoadArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Setup(args) => setup::run(args),
        Command::Drive(args) => drive::run(args).await,
        Command::Promote(args) => promote::run(args).await,
        Command::Load(args) => load::run(args).await,
    }
}
