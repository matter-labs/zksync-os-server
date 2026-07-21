//! Transaction load against a running chaos cluster.
//!
//! Deterministic accounts are funded once through a real L1→L2 deposit (the
//! bridgehub path, signed by anvil's default rich account), then a profile-led
//! mix of workloads drives traffic at a configured rate and shape. Under
//! chaos, submission errors are expected — validators go down mid-run — so
//! failures are counted, never fatal; the point is to put real, varied traffic
//! through consensus while the driver and watcher do their work, and to audit
//! afterwards that the chain did the right thing with all of it.
//!
//! The moving parts, one file each:
//! - [`profile`]: which workloads, at what weights, at what rate (TOML);
//! - [`bank`]: accounts, funding, fees, wedged-nonce rescue;
//! - [`contracts`]: workload contract deployment and typed call encoding;
//! - [`workloads`]: the payload factories, plus the nonce-race saga;
//! - [`engine`]: the tick loop that turns payloads into submissions;
//! - [`stats`]: counting and the end-of-run expectation audit.

mod bank;
mod contracts;
mod engine;
mod profile;
mod stats;
mod workloads;

use crate::setup::Manifest;
use anyhow::Context as _;
use clap::Args;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Args)]
pub struct LoadArgs {
    /// Work directory produced by `chaos setup` (with a running cluster).
    #[arg(long)]
    pub workdir: PathBuf,
    /// Traffic profile: a built-in name (default, realistic, guzzler, quiet,
    /// smoke) or a path to a TOML file. See tools/chaos/profiles/.
    #[arg(long, default_value = "default")]
    pub profile: String,
    /// Transactions per second while sending (overrides the profile).
    #[arg(long)]
    pub tps: Option<u32>,
    /// How long to run; omit to run until interrupted.
    #[arg(long)]
    pub duration: Option<humantime::Duration>,
    /// Traffic shape over time (overrides the profile).
    #[arg(long, value_enum)]
    pub pattern: Option<Pattern>,
    /// Seconds of sending per burst (bursts pattern only; overrides the profile).
    #[arg(long)]
    pub burst_secs: Option<u64>,
    /// Seconds of silence between bursts (bursts pattern only; overrides the
    /// profile).
    #[arg(long)]
    pub idle_secs: Option<u64>,
    /// Where transactions go: `even` (senders pinned round-robin over all
    /// validators) or `single:<i>` (everything to validator i).
    #[arg(long, default_value = "even")]
    pub spread: String,
    /// Number of sender accounts (also the submission parallelism).
    #[arg(long, default_value_t = 8)]
    pub senders: usize,
    /// Seed for deriving deterministic sender keys.
    #[arg(long, default_value_t = 7777)]
    pub key_seed: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Pattern {
    Sustained,
    Bursts,
}

pub async fn run(args: LoadArgs) -> anyhow::Result<()> {
    let manifest: Manifest = serde_json::from_str(&std::fs::read_to_string(
        args.workdir.join("manifest.json"),
    )?)?;
    let validators = manifest.validators.len();
    let target_of = parse_spread(&args.spread, validators)?;

    // The profile decides the mix; explicit flags override its shape knobs.
    let mut profile = profile::resolve(&args.profile)?;
    if let Some(tps) = args.tps {
        profile.tps = tps;
    }
    if let Some(pattern) = args.pattern {
        profile.pattern = match pattern {
            Pattern::Sustained => "sustained".to_string(),
            Pattern::Bursts => "bursts".to_string(),
        };
    }
    if let Some(burst_secs) = args.burst_secs {
        profile.burst_secs = burst_secs;
    }
    if let Some(idle_secs) = args.idle_secs {
        profile.idle_secs = idle_secs;
    }

    let mut bank = bank::Bank::build(&manifest, &target_of, args.senders, args.key_seed)
        .await
        .context("building accounts")?;
    bank.fund(&manifest).await?;

    let mut deployments = contracts::Deployments::open(&args.workdir);
    let mut workloads = workloads::build_enabled(&profile, &mut bank, &mut deployments).await?;

    let stats = Arc::new(Mutex::new(stats::LoadStats::new(validators)));

    // Sagas run beside the tick loop and stop when it does. The L1-flow sagas
    // each get their own funded L1 account (concurrent sagas must never race
    // one key's nonces) and are built sequentially — construction funds
    // accounts through anvil's rich key.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut sagas = Vec::new();
    let rpc_urls: Vec<String> = manifest
        .validators
        .iter()
        .map(|validator| format!("http://127.0.0.1:{}", validator.host_rpc_port))
        .collect();
    let plain_l2 = |url: &String| -> anyhow::Result<alloy::providers::DynProvider> {
        use alloy::providers::{Provider as _, ProviderBuilder};
        Ok(ProviderBuilder::new()
            .connect_http(url.parse().context("validator rpc url")?)
            .erased())
    };
    if let Some(every) = profile.sagas.nonce_race_secs
        && let Some(saga) = workloads::nonce_race::NonceRace::build(&mut bank, &rpc_urls)?
    {
        sagas.push(tokio::spawn(saga.run(
            Duration::from_secs(every),
            stats.clone(),
            shutdown_rx.clone(),
        )));
    }
    if let Some(every) = profile.sagas.deposits_secs {
        let l1 = workloads::l1_support::L1Side::new(
            &manifest,
            bank.chain_id,
            b"chaos-l1-deposits",
            args.key_seed,
            50,
        )
        .await?;
        let saga = workloads::deposits::Deposits::new(l1, plain_l2(&rpc_urls[0])?);
        sagas.push(tokio::spawn(saga.run(
            Duration::from_secs(every),
            stats.clone(),
            shutdown_rx.clone(),
        )));
    }
    if let Some(every) = profile.sagas.withdrawals_secs {
        let l1 = workloads::l1_support::L1Side::new(
            &manifest,
            bank.chain_id,
            b"chaos-l1-withdrawals",
            args.key_seed,
            50,
        )
        .await?;
        let account = bank
            .withdrawer
            .take()
            .context("the withdrawer account is built once")?;
        let saga =
            workloads::withdrawals::Withdrawals::new(l1, account, plain_l2(&rpc_urls[0])?).await?;
        sagas.push(tokio::spawn(saga.run(
            Duration::from_secs(every),
            stats.clone(),
            shutdown_rx.clone(),
        )));
    }
    if let Some(every) = profile.sagas.failed_deposits_secs {
        let l1 = workloads::l1_support::L1Side::new(
            &manifest,
            bank.chain_id,
            b"chaos-l1-failed-deposits",
            args.key_seed,
            50,
        )
        .await?;
        let saga = workloads::failed_deposits::FailedDeposits::new(
            l1,
            plain_l2(&rpc_urls[0])?,
            &mut bank,
            &mut deployments,
        )
        .await?;
        sagas.push(tokio::spawn(saga.run(
            Duration::from_secs(every),
            stats.clone(),
            shutdown_rx.clone(),
        )));
    }

    let names: Vec<&str> = workloads.iter().map(|(w, _)| w.name()).collect();
    println!(
        "loading {} validators with {} senders at {} tps ({}), profile {} [{}], chain id {}",
        validators,
        bank.senders.len(),
        profile.tps,
        args.spread,
        args.profile,
        names.join(", "),
        bank.chain_id,
    );

    let elapsed = engine::run(
        engine::EngineConfig {
            tps: profile.tps,
            bursts: (profile.pattern == "bursts")
                .then_some((profile.burst_secs, profile.idle_secs)),
            duration: args.duration.map(|duration| *duration),
            key_seed: args.key_seed,
        },
        &mut bank,
        &mut workloads,
        &stats,
    )
    .await;

    let _ = shutdown_tx.send(true);
    for saga in sagas {
        let _ = saga.await;
    }

    // Every saga has been joined, so this is the last reference.
    let stats = Arc::try_unwrap(stats)
        .map_err(|_| anyhow::anyhow!("a stats handle outlived the sagas"))?
        .into_inner()
        .unwrap();
    stats::final_report(&stats, &bank, elapsed).await
}

fn parse_spread(spread: &str, validators: usize) -> anyhow::Result<Vec<usize>> {
    // Maps sender index -> validator index; senders beyond the map wrap around.
    if spread == "even" {
        return Ok((0..validators).collect());
    }
    if let Some(index) = spread.strip_prefix("single:") {
        let index: usize = index.parse().context("spread: expected `single:<i>`")?;
        anyhow::ensure!(
            index < validators,
            "spread targets validator {index}, but there are only {validators}"
        );
        return Ok(vec![index]);
    }
    anyhow::bail!("unknown spread {spread:?}; use `even` or `single:<i>`")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_even_and_single_parse() {
        assert_eq!(parse_spread("even", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_spread("single:2", 3).unwrap(), vec![2]);
        assert!(parse_spread("single:5", 3).is_err());
        assert!(parse_spread("sideways", 3).is_err());
    }
}
