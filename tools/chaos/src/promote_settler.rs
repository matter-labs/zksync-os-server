//! Moves the settler role to another committee member — the rig-side execution
//! of the failover runbook (docs/src/consensus/operating.md, "Settlement
//! failover"): demote the old settler's configuration first, then promote the
//! new one by restart.
//!
//! The demote-first order is the safety property, not a style choice: rig nodes
//! share one set of operator keys (per-validator identities are an environment
//! concern), so two live settlers would race the same L1 account's nonces. With
//! per-validator keys the same race is settled by L1 itself — the loser's
//! command reverts and it dies loudly — but the rig never relies on that. If the
//! old settler's container is already dead (the failover drill), its overlay is
//! rewritten in place so any later heal brings it back as a standby, never as a
//! second settler.

use crate::setup::{Manifest, Materials, node_overlay};
use anyhow::Context as _;
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct PromoteSettlerArgs {
    /// Work directory produced by `chaos setup` (with a running cluster).
    #[arg(long)]
    pub workdir: PathBuf,
    /// Validator index to make the settler.
    #[arg(long)]
    pub to: usize,
}

pub async fn run(args: PromoteSettlerArgs) -> anyhow::Result<()> {
    let manifest_path = args.workdir.join("manifest.json");
    let materials_path = args.workdir.join("materials.json");
    let mut manifest: Manifest = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)
        .context("unreadable manifest.json")?;
    let mut materials: Materials = serde_json::from_str(&std::fs::read_to_string(&materials_path)?)
        .context("unreadable materials.json")?;

    let old = materials.settler;
    anyhow::ensure!(args.to != old, "validator {old} is already the settler");
    anyhow::ensure!(
        args.to < materials.scheduled_validators(),
        "validator {} is not a scheduled committee member (observers cannot settle)",
        args.to,
    );

    // Regenerate both overlays from the new materials; the other nodes' files
    // do not depend on who settles.
    materials.settler = args.to;
    for index in [old, args.to] {
        std::fs::write(
            args.workdir
                .join(format!("validator-{index}/validator.yaml")),
            node_overlay(&materials, index),
        )?;
    }
    std::fs::write(&materials_path, serde_json::to_string_pretty(&materials)?)?;

    // Demote first. A dead container keeps its rewritten overlay for whenever it
    // heals; a live one restarts into a standby now.
    let old_name = format!("chaos-{}", manifest.validators[old].name);
    if container_running(&old_name).await? {
        println!("restarting {old_name} as a standby (batcher disabled)");
        docker_restart(&old_name).await?;
    } else {
        println!("{old_name} is down; its configuration is now standby for whenever it returns");
    }

    let new_name = format!("chaos-{}", manifest.validators[args.to].name);
    println!("restarting {new_name} as the settler");
    docker_restart(&new_name).await?;

    manifest.settler = args.to;
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
    println!(
        "settler moved: validator {old} -> validator {}; a `chaos drive` started \
         now watches settlement lag against the new settler",
        args.to,
    );
    Ok(())
}

async fn container_running(name: &str) -> anyhow::Result<bool> {
    let output = tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .output()
        .await
        .context("docker inspect")?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

async fn docker_restart(name: &str) -> anyhow::Result<()> {
    let status = tokio::process::Command::new("docker")
        .args(["restart", name])
        .status()
        .await
        .context("docker restart")?;
    anyhow::ensure!(status.success(), "docker restart {name} failed");
    Ok(())
}
