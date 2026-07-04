//! Generates a chaos cluster's work directory: committee keys, per-validator node
//! configuration overlays, the docker compose file, and the manifest the driver reads.
//!
//! The layout mirrors how a validator is configured anywhere else — the same layered
//! `--config` files the node binary always takes — so the rig runs the exact
//! production artifact, not a test build:
//!
//! ```text
//! <out>/
//!   manifest.json           driver input: names, ports, quorum arithmetic
//!   docker-compose.yaml     anvil + one service per validator
//!   validator-<i>/
//!     validator.yaml        this validator's overlay (keys, ports, db path)
//! ```

use clap::Args;
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::Signer as _;
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::MinPk;
use commonware_cryptography::ed25519;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::PathBuf;

/// The fixed in-container ports; per-validator host mappings are derived from
/// `--base-port` and recorded in the manifest.
const CONTAINER_RPC: u16 = 3050;
const CONTAINER_STATUS: u16 = 3071;
const CONTAINER_PROMETHEUS: u16 = 3312;
const CONTAINER_CONSENSUS: u16 = 3054;

/// Foundry release the repo is validated against (newer anvil cannot load the
/// checked-in L1 state).
const FOUNDRY_IMAGE: &str = "ghcr.io/foundry-rs/foundry:v1.5.1";

#[derive(Args)]
pub struct SetupArgs {
    /// Number of validators. Committees of n tolerate (n-1)/3 faults: 4 is the
    /// smallest that survives losing anyone.
    #[arg(long, default_value_t = 5)]
    pub validators: usize,
    /// Work directory to generate into (created if missing; must be empty).
    #[arg(long)]
    pub out: PathBuf,
    /// Chain directory (config.yaml, genesis) relative to the repo root, which the
    /// compose file mounts read-only into every container.
    #[arg(long, default_value = "local-chains/v30.2/default")]
    pub chain: String,
    /// Node image to run (build one with the repo's Dockerfile).
    #[arg(long, default_value = "zksync-os-server:latest")]
    pub image: String,
    /// First host port; validator i gets rpc/status/metrics at base + 10*i + 0/1/2.
    #[arg(long, default_value_t = 3400)]
    pub base_port: u16,
    /// Repository root (for mounting `local-chains` into the containers).
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
}

/// Everything the driver (and any external monitor) needs to know about the cluster.
#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub validators: Vec<ValidatorEntry>,
    /// The compose network faults detach containers from.
    pub network: String,
    pub quorum: usize,
}

#[derive(Serialize, Deserialize)]
pub struct ValidatorEntry {
    /// Compose service and container name.
    pub name: String,
    pub host_rpc_port: u16,
    pub host_status_port: u16,
    pub host_metrics_port: u16,
}

pub fn run(args: SetupArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.validators >= 2,
        "a committee needs at least 2 validators"
    );
    let repo = args.repo.canonicalize().map_err(|error| {
        anyhow::anyhow!("cannot resolve --repo {}: {error}", args.repo.display())
    })?;
    anyhow::ensure!(
        repo.join(&args.chain).join("config.yaml").exists(),
        "{} does not look like the repository root (missing {}/config.yaml)",
        repo.display(),
        args.chain,
    );
    std::fs::create_dir_all(&args.out)?;
    anyhow::ensure!(
        std::fs::read_dir(&args.out)?.next().is_none(),
        "work directory {} is not empty",
        args.out.display(),
    );

    // Committee keys, exactly as the keygen tool would mint them.
    let mut rng = rand08::rngs::OsRng;
    let mut network_keys = Vec::new();
    let mut bls_keys = Vec::new();
    let mut committee = Vec::new();
    for index in 0..args.validators {
        let mut seed = [0u8; 32];
        rand08::RngCore::fill_bytes(&mut rng, &mut seed);
        let network =
            ed25519::PrivateKey::decode(seed.as_slice()).expect("32 random bytes are a valid key");
        let (bls, bls_public) = ops::keypair::<_, MinPk>(&mut rng);
        committee.push(format!(
            "{}:{}@validator-{index}:{CONTAINER_CONSENSUS}",
            alloy::hex::encode(network.public_key().encode()),
            alloy::hex::encode(bls_public.encode()),
        ));
        network_keys.push(network);
        bls_keys.push(bls);
    }
    // A committee-wide constant (verification pins it).
    let mut fee_collector_bytes = [0u8; 20];
    rand08::RngCore::fill_bytes(&mut rng, &mut fee_collector_bytes);
    let fee_collector = alloy::primitives::Address::from(fee_collector_bytes);

    let mut manifest = Manifest {
        validators: Vec::new(),
        network: "chaos".to_string(),
        quorum: args.validators - (args.validators - 1) / 3,
    };

    for index in 0..args.validators {
        let dir = args.out.join(format!("validator-{index}"));
        std::fs::create_dir_all(&dir)?;
        let overlay = validator_overlay(
            index,
            &alloy::hex::encode(network_keys[index].encode()),
            &alloy::hex::encode(bls_keys[index].encode()),
            &committee,
            fee_collector,
        );
        std::fs::write(dir.join("validator.yaml"), overlay)?;
        manifest.validators.push(ValidatorEntry {
            name: format!("validator-{index}"),
            host_rpc_port: args.base_port + 10 * index as u16,
            host_status_port: args.base_port + 10 * index as u16 + 1,
            host_metrics_port: args.base_port + 10 * index as u16 + 2,
        });
    }

    std::fs::write(
        args.out.join("docker-compose.yaml"),
        compose_file(&args, &repo, &manifest),
    )?;
    std::fs::write(
        args.out.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    println!(
        "generated a {}-validator cluster in {}",
        args.validators,
        args.out.display()
    );
    println!(
        "bring it up:   docker compose -f {}/docker-compose.yaml up -d",
        args.out.display()
    );
    println!(
        "then drive it: chaos drive --workdir {} --seed 42",
        args.out.display()
    );
    Ok(())
}

/// The per-validator configuration overlay, layered on top of the repo's
/// `local_dev.yaml` and the chain's `config.yaml`.
fn validator_overlay(
    index: usize,
    network_key_hex: &str,
    bls_key_hex: &str,
    committee: &[String],
    fee_collector: alloy::primitives::Address,
) -> String {
    let mut yaml = String::new();
    let out = &mut yaml;
    let _ = writeln!(out, "general:");
    let _ = writeln!(out, "  rocks_db_path: /db");
    let _ = writeln!(out, "  node_role: main");
    let _ = writeln!(out, "l1_provider:");
    let _ = writeln!(out, "  rpc_url: ws://anvil:8545");
    let _ = writeln!(out, "rpc:");
    let _ = writeln!(out, "  address: 0.0.0.0:{CONTAINER_RPC}");
    let _ = writeln!(out, "status_server:");
    let _ = writeln!(out, "  address: 0.0.0.0:{CONTAINER_STATUS}");
    let _ = writeln!(out, "observability:");
    let _ = writeln!(out, "  prometheus:");
    let _ = writeln!(out, "    port: {CONTAINER_PROMETHEUS}");
    let _ = writeln!(out, "sequencer:");
    let _ = writeln!(out, "  fee_collector_address: '{fee_collector}'");
    let _ = writeln!(out, "batcher:");
    // Exactly one batcher, like production; the rest are sequencing-only.
    let _ = writeln!(out, "  enabled: {}", index == 0);
    let _ = writeln!(out, "prover_input_generator:");
    let _ = writeln!(out, "  enable_input_generation: false");
    let _ = writeln!(out, "consensus:");
    let _ = writeln!(out, "  enabled: true");
    let _ = writeln!(out, "  network_key: '{network_key_hex}'");
    let _ = writeln!(out, "  bls_key: '{bls_key_hex}'");
    let _ = writeln!(out, "  listen_address: 0.0.0.0:{CONTAINER_CONSENSUS}");
    let _ = writeln!(out, "  allow_private_ips: true");
    let _ = writeln!(out, "  validators:");
    for entry in committee {
        let _ = writeln!(out, "    - '{entry}'");
    }
    yaml
}

fn compose_file(args: &SetupArgs, repo: &std::path::Path, manifest: &Manifest) -> String {
    let repo = repo.display();
    let mut yaml = String::new();
    let out = &mut yaml;
    let _ = writeln!(
        out,
        "# Generated by `chaos setup` — regenerate rather than edit."
    );
    let _ = writeln!(out, "services:");
    let _ = writeln!(out, "  anvil:");
    let _ = writeln!(out, "    image: {FOUNDRY_IMAGE}");
    let _ = writeln!(out, "    container_name: chaos-anvil");
    let _ = writeln!(out, "    entrypoint: [\"sh\", \"-c\"]");
    let _ = writeln!(
        out,
        "    command: [\"gunzip -c /l1-state.json.gz > /tmp/l1-state.json && \
         anvil --host 0.0.0.0 --port 8545 --load-state /tmp/l1-state.json \
         --block-time 0.25 --mixed-mining --slots-in-an-epoch 10\"]"
    );
    let chain_parent = parent_dir(&args.chain);
    let _ = writeln!(out, "    volumes:");
    let _ = writeln!(
        out,
        "      - {repo}/{chain_parent}/l1-state.json.gz:/l1-state.json.gz:ro"
    );
    let _ = writeln!(out, "    networks: [{}]", manifest.network);
    for (index, entry) in manifest.validators.iter().enumerate() {
        let _ = writeln!(out, "  {}:", entry.name);
        let _ = writeln!(out, "    image: {}", args.image);
        let _ = writeln!(out, "    container_name: chaos-{}", entry.name);
        let _ = writeln!(out, "    depends_on: [anvil]");
        let _ = writeln!(
            out,
            "    command: [\"--config\", \"/app/local-chains/local_dev.yaml\", \
             \"--config\", \"/app/{}/config.yaml\", \
             \"--config\", \"/config/validator.yaml\"]",
            args.chain
        );
        let _ = writeln!(out, "    volumes:");
        let _ = writeln!(out, "      - {repo}/local-chains:/app/local-chains:ro");
        let _ = writeln!(out, "      - ./validator-{index}:/config:ro");
        let _ = writeln!(out, "      - chaos-db-{index}:/db");
        let _ = writeln!(out, "    ports:");
        let _ = writeln!(out, "      - \"{}:{CONTAINER_RPC}\"", entry.host_rpc_port);
        let _ = writeln!(
            out,
            "      - \"{}:{CONTAINER_STATUS}\"",
            entry.host_status_port
        );
        let _ = writeln!(
            out,
            "      - \"{}:{CONTAINER_PROMETHEUS}\"",
            entry.host_metrics_port
        );
        let _ = writeln!(out, "    networks: [{}]", manifest.network);
        let _ = writeln!(out, "    restart: \"no\"");
    }
    let _ = writeln!(out, "networks:");
    let _ = writeln!(out, "  {}:", manifest.network);
    let _ = writeln!(out, "    name: {}", manifest.network);
    let _ = writeln!(out, "volumes:");
    for index in 0..manifest.validators.len() {
        let _ = writeln!(out, "  chaos-db-{index}:");
    }
    yaml
}

/// The chain directory's parent (where `l1-state.json.gz` lives), e.g.
/// `local-chains/v30.2` for `local-chains/v30.2/default`.
fn parent_dir(chain: &str) -> String {
    chain
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_else(|| ".".to_string())
}
