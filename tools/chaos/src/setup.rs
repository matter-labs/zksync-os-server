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

/// The compose network's subnet. Validators get static IPs because the node parses
/// committee addresses as `SocketAddr` (numeric only, no DNS), and because a
/// partition heal must restore the exact address the committee knows
/// (`docker network connect` without `--ip` would draw a fresh dynamic one).
const SUBNET: &str = "172.28.0.0/24";

/// Validator `index`'s static IP on [`SUBNET`]. Offset 10 keeps clear of the
/// gateway (.1) and anvil's dynamic address.
fn validator_ip(index: usize) -> String {
    format!("172.28.0.{}", 10 + index)
}

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
    /// Host port anvil (the L1) is published on — `chaos load` funds accounts
    /// through it.
    pub host_l1_port: u16,
    /// The chain's bridgehub on L1, for deposits.
    pub bridgehub_address: String,
}

#[derive(Serialize, Deserialize)]
pub struct ValidatorEntry {
    /// Compose service and container name.
    pub name: String,
    /// Static IP on the compose network; a partition heal reconnects with exactly
    /// this address, since it is what the rest of the committee dials.
    pub ip: String,
    pub host_rpc_port: u16,
    pub host_status_port: u16,
    pub host_metrics_port: u16,
}

pub fn run(args: SetupArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.validators >= 2,
        "a committee needs at least 2 validators"
    );
    anyhow::ensure!(
        args.validators <= 200,
        "the static-IP scheme fits at most 200 validators in {SUBNET}"
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
            "{}:{}@{}:{CONTAINER_CONSENSUS}",
            alloy::hex::encode(network.public_key().encode()),
            alloy::hex::encode(bls_public.encode()),
            validator_ip(index),
        ));
        network_keys.push(network);
        bls_keys.push(bls);
    }
    // A committee-wide constant (verification pins it).
    let mut fee_collector_bytes = [0u8; 20];
    rand08::RngCore::fill_bytes(&mut rng, &mut fee_collector_bytes);
    let fee_collector = alloy::primitives::Address::from(fee_collector_bytes);

    let bridgehub_address = read_bridgehub_address(&repo.join(&args.chain).join("config.yaml"))?;
    let mut manifest = Manifest {
        validators: Vec::new(),
        network: "chaos".to_string(),
        quorum: args.validators - (args.validators - 1) / 3,
        // First free slot after the per-validator port blocks.
        host_l1_port: args.base_port + 10 * args.validators as u16,
        bridgehub_address,
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
            ip: validator_ip(index),
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
    let committee_entries: String = committee
        .iter()
        .map(|entry| format!("    - '{entry}'\n"))
        .collect();
    // Exactly one batcher, like production; the rest are sequencing-only. The
    // paths under `/db` move config defaults off the image's read-only workdir.
    let batcher_enabled = index == 0;
    format!(
        "\
general:
  rocks_db_path: /db
  node_role: main
l1_provider:
  rpc_url: http://anvil:8545
rpc:
  address: 0.0.0.0:{CONTAINER_RPC}
status_server:
  address: 0.0.0.0:{CONTAINER_STATUS}
observability:
  prometheus:
    port: {CONTAINER_PROMETHEUS}
sequencer:
  fee_collector_address: '{fee_collector}'
  block_dump_path: /db/block_dumps
batcher:
  enabled: {batcher_enabled}
prover_api:
  proof_storage:
    path: /db/fri_proofs
prover_input_generator:
  enable_input_generation: false
consensus:
  enabled: true
  network_key: '{network_key_hex}'
  bls_key: '{bls_key_hex}'
  listen_address: 0.0.0.0:{CONTAINER_CONSENSUS}
  allow_private_ips: true
  validators:
{committee_entries}"
    )
}

fn compose_file(args: &SetupArgs, repo: &std::path::Path, manifest: &Manifest) -> String {
    let repo = repo.display();
    let network = &manifest.network;
    let chain = &args.chain;
    let chain_parent = parent_dir(chain);
    let image = &args.image;
    let host_l1_port = manifest.host_l1_port;

    let validators: String = manifest
        .validators
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            // NET_ADMIN lets the driver inject `tc netem` degradation; the
            // json-file caps keep `docker logs` cost flat on multi-hour runs.
            format!(
                "  {name}:
    image: {image}
    container_name: chaos-{name}
    depends_on: [anvil]
    command: [\"--config\", \"/app/local-chains/local_dev.yaml\", \"--config\", \"/app/{chain}/config.yaml\", \"--config\", \"/config/validator.yaml\"]
    volumes:
      - {repo}/local-chains:/app/local-chains:ro
      - ./validator-{index}:/config:ro
      - chaos-db-{index}:/db
    ports:
      - \"{rpc}:{CONTAINER_RPC}\"
      - \"{status}:{CONTAINER_STATUS}\"
      - \"{metrics}:{CONTAINER_PROMETHEUS}\"
    networks:
      {network}:
        ipv4_address: {ip}
    cap_add: [NET_ADMIN]
    logging:
      driver: json-file
      options:
        max-size: \"50m\"
        max-file: \"3\"
    restart: \"no\"
",
                name = entry.name,
                rpc = entry.host_rpc_port,
                status = entry.host_status_port,
                metrics = entry.host_metrics_port,
                ip = entry.ip,
            )
        })
        .collect();

    let volumes: String = (0..manifest.validators.len())
        .map(|index| format!("  chaos-db-{index}:\n"))
        .collect();

    format!(
        "\
# Generated by `chaos setup` — regenerate rather than edit.
services:
  anvil:
    image: {FOUNDRY_IMAGE}
    container_name: chaos-anvil
    entrypoint: [\"sh\", \"-c\"]
    command: [\"gunzip -c /l1-state.json.gz > /tmp/l1-state.json && anvil --host 0.0.0.0 --port 8545 --load-state /tmp/l1-state.json --block-time 0.25 --mixed-mining --slots-in-an-epoch 10\"]
    volumes:
      - {repo}/{chain_parent}/l1-state.json.gz:/l1-state.json.gz:ro
    ports:
      - \"{host_l1_port}:8545\"
    networks: [{network}]
    logging:
      driver: json-file
      options:
        max-size: \"50m\"
        max-file: \"3\"
{validators}networks:
  {network}:
    name: {network}
    ipam:
      config:
        - subnet: {SUBNET}
volumes:
{volumes}"
    )
}

/// Pulls `l1.bridgehub_address` out of the chain's `config.yaml` without dragging
/// in a YAML parser: the in-repo format is stable and the key is unique.
fn read_bridgehub_address(config_path: &std::path::Path) -> anyhow::Result<String> {
    let config = std::fs::read_to_string(config_path)?;
    config
        .lines()
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed.strip_prefix("bridgehub_address:").map(|value| {
                value
                    .trim()
                    .trim_matches('\'')
                    .trim_matches('"')
                    .to_string()
            })
        })
        .filter(|address| !address.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no bridgehub_address found in {}", config_path.display()))
}

/// The chain directory's parent (where `l1-state.json.gz` lives), e.g.
/// `local-chains/v30.2` for `local-chains/v30.2/default`.
fn parent_dir(chain: &str) -> String {
    chain
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_else(|| ".".to_string())
}
