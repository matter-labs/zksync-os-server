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
    /// Consensus epoch length in blocks for the cluster. The node's default is
    /// hours-scale; soaks that should cross many epoch boundaries (rotation and
    /// reconfiguration hunting) pass a few hundred.
    #[arg(long, default_value_t = 600)]
    pub epoch_length: u64,
    /// Number of non-voting observers beside the committee. Each gets a BLS key
    /// minted (unused until then) so `chaos promote` can schedule it into the
    /// committee later.
    #[arg(long, default_value_t = 0)]
    pub observers: usize,
    /// Retired epochs of consensus storage each node keeps (0 = keep everything).
    /// Soaks measuring disk growth run short epochs with a small retention.
    #[arg(long, default_value_t = 0)]
    pub epoch_retention: u64,
    /// Extra environment variables for every validator container, KEY=VALUE
    /// (repeatable). The escape hatch for investigation runs — e.g. activating
    /// heap profiling on a `jemalloc-profiling` image with
    /// `--node-env MALLOC_CONF=prof:true,prof_final:true,prof_prefix:/db/jeprof`.
    #[arg(long = "node-env")]
    pub node_env: Vec<String>,
}

/// Everything the driver (and any external monitor) needs to know about the cluster.
#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub validators: Vec<ValidatorEntry>,
    /// Non-voting observers on the consensus network. The driver leaves them
    /// alone (they carry no quorum weight); `chaos promote` moves entries from
    /// here into `validators` and rewrites `quorum`.
    #[serde(default)]
    pub observers: Vec<ValidatorEntry>,
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

/// Everything `chaos promote` needs to regenerate node overlays: the cluster's
/// key material and shared constants, written by setup as `materials.json`.
/// (Private keys in a work directory are rig-grade security — the same keys are
/// already in the overlays themselves.)
#[derive(Serialize, Deserialize)]
pub struct Materials {
    pub nodes: Vec<NodeMaterials>,
    /// The committee schedule as prefix sizes: the entry activating at
    /// `activation_epoch` consists of `nodes[..validators]`. Setup writes a
    /// single epoch-0 entry; `chaos promote` appends grown ones — promotion
    /// always takes the next observers in index order, so prefixes describe
    /// every committee this rig can express.
    pub schedule: Vec<ScheduleStep>,
    pub fee_collector: String,
    pub epoch_length: u64,
    /// Retired epochs of consensus storage each node keeps (0 = keep everything).
    #[serde(default)]
    pub epoch_retention: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct ScheduleStep {
    pub activation_epoch: u64,
    pub validators: usize,
}

impl Materials {
    /// Nodes that hold (or will hold) a committee seat — validator-role nodes.
    pub fn scheduled_validators(&self) -> usize {
        self.schedule
            .last()
            .expect("a schedule always has its epoch-0 entry")
            .validators
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct NodeMaterials {
    pub network_key_hex: String,
    pub bls_key_hex: String,
    /// `<network_pub>:<bls_pub>@<ip>:<port>` — this node's entry in any committee
    /// that includes it.
    pub committee_entry: String,
    /// `<network_pub>@<ip>:<port>` — this node's admission-list entry while it
    /// observes.
    pub observer_entry: String,
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

    // Keys for every node — committee and observers alike, exactly as the keygen
    // tool would mint them. An observer's BLS key is promotion material: unused
    // until `chaos promote` schedules the node into the committee.
    let total = args.validators + args.observers;
    let mut rng = rand08::rngs::OsRng;
    let mut nodes = Vec::new();
    for index in 0..total {
        let mut seed = [0u8; 32];
        rand08::RngCore::fill_bytes(&mut rng, &mut seed);
        let network =
            ed25519::PrivateKey::decode(seed.as_slice()).expect("32 random bytes are a valid key");
        let (bls, bls_public) = ops::keypair::<_, MinPk>(&mut rng);
        let network_pub = alloy::hex::encode(network.public_key().encode());
        nodes.push(NodeMaterials {
            network_key_hex: alloy::hex::encode(network.encode()),
            bls_key_hex: alloy::hex::encode(bls.encode()),
            committee_entry: format!(
                "{network_pub}:{}@{}:{CONTAINER_CONSENSUS}",
                alloy::hex::encode(bls_public.encode()),
                validator_ip(index),
            ),
            observer_entry: format!(
                "{network_pub}@{}:{CONTAINER_CONSENSUS}",
                validator_ip(index)
            ),
        });
    }
    // A committee-wide constant (verification pins it).
    let mut fee_collector_bytes = [0u8; 20];
    rand08::RngCore::fill_bytes(&mut rng, &mut fee_collector_bytes);
    let fee_collector = alloy::primitives::Address::from(fee_collector_bytes);
    let materials = Materials {
        nodes,
        schedule: vec![ScheduleStep {
            activation_epoch: 0,
            validators: args.validators,
        }],
        fee_collector: format!("{fee_collector}"),
        epoch_length: args.epoch_length,
        epoch_retention: args.epoch_retention,
    };

    let bridgehub_address = read_bridgehub_address(&repo.join(&args.chain).join("config.yaml"))?;
    let mut manifest = Manifest {
        validators: Vec::new(),
        observers: Vec::new(),
        network: "chaos".to_string(),
        quorum: args.validators - (args.validators - 1) / 3,
        // First free slot after the per-node port blocks.
        host_l1_port: args.base_port + 10 * total as u16,
        bridgehub_address,
    };

    for index in 0..total {
        let dir = args.out.join(format!("validator-{index}"));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("validator.yaml"), node_overlay(&materials, index))?;
        let entry = ValidatorEntry {
            name: format!("validator-{index}"),
            ip: validator_ip(index),
            host_rpc_port: args.base_port + 10 * index as u16,
            host_status_port: args.base_port + 10 * index as u16 + 1,
            host_metrics_port: args.base_port + 10 * index as u16 + 2,
        };
        if index < args.validators {
            manifest.validators.push(entry);
        } else {
            manifest.observers.push(entry);
        }
    }
    std::fs::write(
        args.out.join("materials.json"),
        serde_json::to_string_pretty(&materials)?,
    )?;

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

/// One node's configuration overlay, layered on top of the repo's
/// `local_dev.yaml` and the chain's `config.yaml` — regenerable at any time from
/// the [`Materials`], which is how `chaos promote` rewrites the cluster.
///
/// The committee is expressed as a `committees` schedule (a single epoch-0 entry
/// at setup); promotion appends entries. Observers get `role: observer`, no BLS
/// key, and transaction forwarding to every committee member's in-network RPC.
pub fn node_overlay(materials: &Materials, index: usize) -> String {
    // A node is validator-role as soon as any schedule entry seats it: it needs
    // its signing key deployed before its activation epoch arrives (it simply
    // runs no engine until then).
    let scheduled_validators = materials.scheduled_validators();
    let is_validator = index < scheduled_validators;
    let fee_collector = &materials.fee_collector;
    let epoch_length = materials.epoch_length;
    let epoch_retention = materials.epoch_retention;
    let node = &materials.nodes[index];
    let network_key_hex = &node.network_key_hex;

    let committee_lines = |members: &[NodeMaterials]| -> String {
        members
            .iter()
            .map(|member| format!("        - '{}'\n", member.committee_entry))
            .collect()
    };
    let committees: String = std::iter::once("  committees:\n".to_string())
        .chain(materials.schedule.iter().map(|step| {
            format!(
                "    - activation_epoch: {}\n      validators:\n{}",
                step.activation_epoch,
                committee_lines(&materials.nodes[..step.validators])
            )
        }))
        .collect();
    let observer_lines: String = materials.nodes[scheduled_validators..]
        .iter()
        .map(|node| format!("    - '{}'\n", node.observer_entry))
        .collect();
    let observers = if observer_lines.is_empty() {
        String::new()
    } else {
        format!("  observers:\n{observer_lines}")
    };

    // Exactly one batcher, like production; the rest are sequencing-only. The
    // paths under `/db` move config defaults off the image's read-only workdir.
    let batcher_enabled = index == 0;
    let role_section = if is_validator {
        format!("  bls_key: '{}'\n", node.bls_key_hex)
    } else {
        let forward_urls: String = (0..materials.schedule[0].validators)
            .map(|validator| {
                format!(
                    "    - 'http://{}:{CONTAINER_RPC}'\n",
                    validator_ip(validator)
                )
            })
            .collect();
        format!("  role: observer\n  tx_forward_rpc_urls:\n{forward_urls}")
    };
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
{role_section}  listen_address: 0.0.0.0:{CONTAINER_CONSENSUS}
  allow_private_ips: true
  epoch_length: {epoch_length}
  epoch_retention: {epoch_retention}
  # Rig clusters pin the legacy idle behavior (constant empty blocks): the
  # watcher's liveness window and the soak metrics assume steady progress.
  # Idle-policy experiments override this deliberately.
  idle_heartbeat: 0s
{committees}{observers}"
    )
}

fn compose_file(args: &SetupArgs, repo: &std::path::Path, manifest: &Manifest) -> String {
    let repo = repo.display();
    let network = &manifest.network;
    let chain = &args.chain;
    let chain_parent = parent_dir(chain);
    let image = &args.image;
    let host_l1_port = manifest.host_l1_port;
    let environment = if args.node_env.is_empty() {
        String::new()
    } else {
        let lines: String = args
            .node_env
            .iter()
            .map(|pair| format!("      - \"{pair}\"\n"))
            .collect();
        format!("    environment:\n{lines}")
    };

    let validators: String = manifest
        .validators
        .iter()
        .chain(&manifest.observers)
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
{environment}    volumes:
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
                environment = environment,
            )
        })
        .collect();

    let volumes: String = (0..manifest.validators.len() + manifest.observers.len())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn materials(validators: usize, total: usize) -> Materials {
        Materials {
            nodes: (0..total)
                .map(|index| NodeMaterials {
                    network_key_hex: format!("net-priv-{index}"),
                    bls_key_hex: format!("bls-priv-{index}"),
                    committee_entry: format!(
                        "netpub{index}:blspub{index}@172.28.0.{}:3054",
                        10 + index
                    ),
                    observer_entry: format!("netpub{index}@172.28.0.{}:3054", 10 + index),
                })
                .collect(),
            schedule: vec![ScheduleStep {
                activation_epoch: 0,
                validators,
            }],
            fee_collector: "0x0000000000000000000000000000000000000001".into(),
            epoch_length: 240,
            epoch_retention: 0,
        }
    }

    #[test]
    fn overlays_split_roles_and_promotion_regenerates_them() {
        let mut cluster = materials(4, 7);

        // At setup: node 0 is a batcher validator, node 4 an observer.
        let validator = node_overlay(&cluster, 0);
        assert!(validator.contains("bls_key: 'bls-priv-0'"));
        assert!(!validator.contains("role: observer"));
        assert!(validator.contains("activation_epoch: 0"));
        let observer = node_overlay(&cluster, 4);
        assert!(observer.contains("role: observer"));
        assert!(
            !observer.contains("bls_key"),
            "observers hold no signing key"
        );
        assert!(observer.contains("tx_forward_rpc_urls"));
        // Everyone carries the admission list with all three observers.
        for overlay in [&validator, &observer] {
            assert!(overlay.contains("netpub4@"));
            assert!(overlay.contains("netpub6@"));
        }

        // Promotion appends a schedule entry seating the next three observers;
        // regenerated overlays flip their role and shrink the admission list.
        cluster.schedule.push(ScheduleStep {
            activation_epoch: 12,
            validators: 7,
        });
        let promoted = node_overlay(&cluster, 4);
        assert!(promoted.contains("bls_key: 'bls-priv-4'"));
        assert!(!promoted.contains("role: observer"));
        assert!(promoted.contains("activation_epoch: 0"));
        assert!(promoted.contains("activation_epoch: 12"));
        // The grown entry lists all seven; the epoch-0 entry keeps four.
        assert!(promoted.contains("netpub6:blspub6@"));
        // Nobody is on the admission list anymore.
        assert!(!promoted.contains("observers:"));
        // A sitting validator sees the same schedule.
        let sitting = node_overlay(&cluster, 0);
        assert!(sitting.contains("activation_epoch: 12"));
        assert!(!sitting.contains("observers:"));
    }
}
