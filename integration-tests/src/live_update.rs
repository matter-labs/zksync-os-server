//! Infrastructure for the live-DB upgrade test.
//!
//! Downloads a DB snapshot and config from a running Kubernetes cluster, caches them
//! locally, and provides helpers for the two-phase upgrade test:
//!   - Phase 1: old binary (downloaded from GitHub releases) against snapshot DB
//!   - Phase 2: new in-process server against the same DB after phase 1
//!
//! ## Required environment variables
//! - `LIVE_UPDATE_NAMESPACE`   – Kubernetes namespace (e.g. `testnet-alpha`)
//! - `LIVE_UPDATE_POD`         – Kubernetes pod name   (e.g. `sequencer-c-0`)
//! - `LIVE_UPDATE_L1_RPC_URL`  – Publicly accessible L1 RPC URL for forking (e.g. Sepolia Infura)
//!
//! ## Optional environment variables
//! - `LIVE_UPDATE_ARTIFACTS_DIR` – Override cache location (default: `<workspace-root>/live-update-cache/<ns>/<pod>/`,
//!   where workspace root is the parent of the `integration-tests/` crate directory)
//! - `LIVE_UPDATE_OLD_BIN`       – Skip GitHub download, use this binary path instead
//! - `LIVE_UPDATE_L2_WALLET_SK`  – Private key (hex) of a funded L2 account for test transactions
//!
//! ## Prerequisites
//! The following tools must be in PATH:
//! - `anvil` (foundry) – used to fork the real L1

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::providers::ext::AnvilApi;
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret};
use kube::Client;
use kube::api::{Api, AttachParams};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::utils::LockedPort;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const REMOTE_SNAPSHOT_DIR: &str = "/tmp/db-snapshot";

/// Shell script that runs *inside* the cluster pod.
/// Creates a consistent RocksDB snapshot by capturing SST files first, then metadata.
const POD_SNAPSHOT_SCRIPT: &str = r#"set -euo pipefail

rm -rf /tmp/db-snapshot
mkdir -p /tmp/db-snapshot/node1

# Copy top-level dirs (everything except node1)
find /db -mindepth 1 -maxdepth 1 ! -name node1 \
  -exec cp -a {} /tmp/db-snapshot/ \;

# Copy node1 contents except large RocksDB dirs (handled separately below)
find /db/node1 -mindepth 1 -maxdepth 1 \
  ! -name block_replay_wal ! -name tree ! -name repository \
  ! -name state_full_diffs ! -name preimages_full_diffs \
  -exec cp -a {} /tmp/db-snapshot/node1/ \;

# Snapshot each RocksDB dir:
#   1. SST files first  (immutable — point-in-time consistent)
#   2. CURRENT + MANIFEST + OPTIONS + LOG  (metadata, safe after SSTs)
for rel in \
  fri_proofs \
  node1/block_replay_wal \
  node1/tree \
  node1/repository \
  node1/state_full_diffs \
  node1/preimages_full_diffs
do
  SRC="/db/$rel"
  DST="/tmp/db-snapshot/$rel"

  mkdir -p "$DST"
  cd "$SRC"

  tar -cf - --exclude=CURRENT --exclude='MANIFEST-*' --exclude='OPTIONS-*' . | tar -C "$DST" -xf -
  tar -cf - CURRENT MANIFEST-* OPTIONS-* LOG* 2>/dev/null | tar -C "$DST" -xf - 2>/dev/null || true
done

# Verify required dirs have SST files + metadata
for rel in \
  node1/block_replay_wal \
  node1/tree \
  node1/repository \
  node1/state_full_diffs \
  node1/preimages_full_diffs
do
  test -n "$(find "/tmp/db-snapshot/$rel" -maxdepth 1 -name '*.sst' -print -quit)" \
    || { echo "ERROR: no SST files in $rel"; exit 1; }
  test -n "$(find "/tmp/db-snapshot/$rel" -maxdepth 1 \( -name CURRENT -o -name 'MANIFEST-*' \) -print -quit)" \
    || { echo "ERROR: no CURRENT/MANIFEST in $rel"; exit 1; }
done
"#;

// ---------------------------------------------------------------------------
// LiveUpdateConfig
// ---------------------------------------------------------------------------

/// Test configuration derived from environment variables.
#[derive(Debug, Clone)]
pub struct LiveUpdateConfig {
    pub namespace: String,
    pub pod: String,
    /// L1 RPC URL for forking (must be publicly accessible from this machine).
    pub l1_rpc_url: String,
    /// Root directory for cached artifacts.
    pub artifacts_dir: PathBuf,
    /// If set, skip GitHub binary download and use this path for the old server.
    pub old_bin_override: Option<PathBuf>,
    /// Private key of a funded L2 account for generating test transactions (optional).
    pub l2_wallet_sk: Option<String>,
}

impl LiveUpdateConfig {
    /// Returns `None` if any required env var is missing — the test will be skipped.
    pub fn from_env() -> Option<Self> {
        let namespace = std::env::var("LIVE_UPDATE_NAMESPACE").ok()?;
        let pod = std::env::var("LIVE_UPDATE_POD").ok()?;
        let l1_rpc_url = std::env::var("LIVE_UPDATE_L1_RPC_URL").ok()?;

        let artifacts_dir = std::env::var("LIVE_UPDATE_ARTIFACTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                // CARGO_MANIFEST_DIR is set at compile time by cargo and points to this
                // crate's directory (integration-tests/); its parent is the workspace root.
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .expect("CARGO_MANIFEST_DIR has no parent")
                    .join("live-update-cache")
                    .join(&namespace)
                    .join(&pod)
            });

        let old_bin_override = std::env::var("LIVE_UPDATE_OLD_BIN").ok().map(PathBuf::from);
        let l2_wallet_sk = std::env::var("LIVE_UPDATE_L2_WALLET_SK").ok();

        tracing::info!(
            namespace,
            pod,
            artifacts_dir = %artifacts_dir.display(),
            old_bin_override = ?old_bin_override,
            has_wallet_sk = l2_wallet_sk.is_some(),
            "Live-update test config loaded from environment"
        );

        Some(Self {
            namespace,
            pod,
            l1_rpc_url,
            artifacts_dir,
            old_bin_override,
            l2_wallet_sk,
        })
    }
}

// ---------------------------------------------------------------------------
// LiveUpdateArtifacts
// ---------------------------------------------------------------------------

/// Persistent artifacts downloaded from the cluster.
/// The cache is invalidated when the pod's container image tag changes.
pub struct LiveUpdateArtifacts {
    /// Root cache directory.
    pub dir: PathBuf,
    /// Pristine DB snapshot — never modified; copied to a run-dir before each test.
    pub pristine_db: PathBuf,
    /// Cluster genesis JSON.
    pub genesis_json: PathBuf,
    /// Cluster common config YAML (feeds both old binary and new in-process server).
    pub config_yaml: PathBuf,
    /// Old server binary (from cache or `LIVE_UPDATE_OLD_BIN`).
    pub old_bin: PathBuf,
    /// Container image tag at snapshot time.
    pub image_tag: String,
}

impl LiveUpdateArtifacts {
    /// Ensures all artifacts are present and up-to-date.
    /// Downloads from the cluster on cache miss or image-tag change.
    pub async fn ensure(config: &LiveUpdateConfig, kube: &Client) -> anyhow::Result<Self> {
        let dir = &config.artifacts_dir;
        let image_tag = get_pod_image_tag(kube, &config.namespace, &config.pod).await?;

        let tag_file = dir.join("image-tag");
        let cached_tag = fs::read_to_string(&tag_file).ok();

        let pristine_db = dir.join("db");
        let genesis_json = dir.join("genesis.json");
        let config_yaml = dir.join("config.yaml");
        let old_bin = dir.join("old-server");

        let files_present = pristine_db.exists()
            && genesis_json.exists()
            && config_yaml.exists()
            && (old_bin.exists() || config.old_bin_override.is_some());
        let tag_matches = cached_tag.as_deref() == Some(image_tag.as_str());

        if files_present && tag_matches {
            tracing::info!(
                dir = %dir.display(),
                image_tag,
                db = %pristine_db.display(),
                old_bin = %old_bin.display(),
                "Using cached artifacts (image tag matches)"
            );
        } else {
            if files_present && !tag_matches {
                tracing::info!(
                    old_tag = ?cached_tag,
                    new_tag = image_tag,
                    dir = %dir.display(),
                    "Image tag changed — wiping stale cache and re-downloading"
                );
                fs::remove_dir_all(dir).context("failed to remove stale artifacts dir")?;
            } else {
                tracing::info!(
                    dir = %dir.display(),
                    "Artifacts not cached — downloading from cluster"
                );
            }
            fs::create_dir_all(dir).context("failed to create artifacts dir")?;

            tracing::info!("Step 1/4: Downloading DB snapshot from pod...");
            download_db_snapshot(kube, &config.namespace, &config.pod, &pristine_db).await?;

            tracing::info!("Step 2/4: Downloading genesis config...");
            download_configmap_key(
                kube,
                &config.namespace,
                "genesis-config",
                "genesis.json",
                &genesis_json,
            )
            .await?;

            tracing::info!("Step 3/4: Downloading sequencer common config...");
            download_configmap_key(
                kube,
                &config.namespace,
                "sequencer-config-common",
                "common.yaml",
                &config_yaml,
            )
            .await?;

            if config.old_bin_override.is_none() {
                tracing::info!("Step 4/4: Downloading old server binary from GitHub...");
                download_old_binary(&image_tag, &old_bin).await?;
            } else {
                tracing::info!(
                    path = ?config.old_bin_override,
                    "Step 4/4: Skipping binary download (LIVE_UPDATE_OLD_BIN is set)"
                );
            }

            fs::write(&tag_file, &image_tag).context("failed to write image-tag file")?;
            tracing::info!(
                dir = %dir.display(),
                image_tag,
                "All artifacts downloaded and cached successfully"
            );
        }

        let resolved_bin = config.old_bin_override.clone().unwrap_or(old_bin);
        tracing::info!(
            db       = %pristine_db.display(),
            genesis  = %genesis_json.display(),
            config   = %config_yaml.display(),
            old_bin  = %resolved_bin.display(),
            "Artifact paths resolved"
        );

        Ok(Self {
            dir: dir.clone(),
            pristine_db,
            genesis_json,
            config_yaml,
            old_bin: resolved_bin,
            image_tag,
        })
    }
}

// ---------------------------------------------------------------------------
// OperatorKeys
// ---------------------------------------------------------------------------

/// Operator signing keys decoded from the `sequencer` Kubernetes secret.
/// **Never written to disk** — fetched fresh on every test run.
pub struct OperatorKeys {
    /// All decoded key→value pairs from the secret.
    /// Passed as env vars to the old server binary and as a config source for the new server.
    pub all: HashMap<String, String>,
}

impl OperatorKeys {
    pub async fn fetch(kube: &Client, namespace: &str) -> anyhow::Result<Self> {
        tracing::info!(
            namespace,
            "Fetching operator keys from k8s secret (never persisted)"
        );

        let secrets: Api<Secret> = Api::namespaced(kube.clone(), namespace);
        let secret = secrets
            .get("sequencer")
            .await
            .context("failed to get 'sequencer' secret")?;

        let all = secret
            .data
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                let s = String::from_utf8(v.0)
                    .with_context(|| format!("secret key '{k}' is not valid UTF-8"))?;
                Ok((k, s))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;

        let key_names: Vec<&str> = all.keys().map(|s| s.as_str()).collect();
        tracing::info!(count = all.len(), keys = ?key_names, "Fetched operator secret keys");
        Ok(Self { all })
    }
}

// ---------------------------------------------------------------------------
// LiveUpdateRunDir
// ---------------------------------------------------------------------------

/// Per-test-run directory holding a working DB copy and log files.
/// Run dirs are never auto-deleted so you can inspect them after failures.
pub struct LiveUpdateRunDir {
    pub dir: PathBuf,
    /// Working copy of the DB, shared between old and new server.
    pub db: PathBuf,
    logs_dir: PathBuf,
}

impl LiveUpdateRunDir {
    pub fn create(artifacts_dir: &Path) -> anyhow::Result<Self> {
        // Use nanoseconds to avoid collisions when tests run in quick succession.
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = artifacts_dir.join("runs").join(ts.to_string());
        let logs_dir = dir.join("logs");
        let db = dir.join("db");
        fs::create_dir_all(&logs_dir).context("failed to create logs dir")?;
        fs::create_dir_all(&db).context("failed to create run db dir")?;
        tracing::info!(
            run_dir  = %dir.display(),
            logs_dir = %logs_dir.display(),
            db_dir   = %db.display(),
            "Run directory created"
        );
        Ok(Self { dir, db, logs_dir })
    }

    pub fn log_path(&self, name: &str) -> PathBuf {
        self.logs_dir.join(format!("{name}.log"))
    }
}

// ---------------------------------------------------------------------------
// ForkedAnvilL1
// ---------------------------------------------------------------------------

/// Anvil instance forked from the real L1 at its current block.
pub struct ForkedAnvilL1 {
    /// HTTP endpoint for connecting servers.
    pub address: String,
    // Keep process + port alive for the test duration.
    _child: tokio::process::Child,
    _port: LockedPort,
}

impl ForkedAnvilL1 {
    pub async fn start(l1_rpc_url: &str, log_path: &Path) -> anyhow::Result<Self> {
        // Query the current L1 block to fork at.
        tracing::info!(
            l1_rpc_url,
            "Connecting to real L1 to determine fork block..."
        );
        let real_l1 = ProviderBuilder::new()
            .connect(l1_rpc_url)
            .await
            .context("failed to connect to real L1 RPC")?;
        let fork_block = real_l1.get_block_number().await?;
        tracing::info!(fork_block, l1_rpc_url, "Forking L1 at current tip");

        let locked_port = LockedPort::acquire_unused().await?;
        let port = locked_port.port;

        let log_file = File::create(log_path).context("failed to create anvil log")?;
        let log_file2 = log_file.try_clone()?;

        tracing::info!(
            port,
            fork_block,
            log = %log_path.display(),
            "Spawning forked anvil"
        );

        let child = Command::new("anvil")
            .args([
                "--port",
                &port.to_string(),
                "--fork-url",
                l1_rpc_url,
                "--fork-block-number",
                &fork_block.to_string(),
            ])
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file2))
            .spawn()
            .context("failed to spawn anvil — is foundry installed?")?;

        // Wait for anvil to be ready by retrying connect + get_chain_id,
        // matching the pattern used by AnvilL1::start() in the shared test infrastructure.
        // A TCP-only check is not enough: the WebSocket handshake may fail while the
        // process is still initialising.
        let ws_url = format!("ws://localhost:{port}");
        let tmp_provider = (|| async {
            ProviderBuilder::new()
                .connect(&ws_url)
                .await
                .context("failed to connect to forked anvil")
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_millis(200))
                .with_max_times(50),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::info!(%err, ?dur, "retrying connection to forked anvil");
        })
        .await
        .context("forked anvil did not start in time")?;
        tmp_provider
            .get_chain_id()
            .await
            .context("forked anvil did not respond to get_chain_id")?;

        // Advance timestamp to wall-clock now and mine one block.

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        tmp_provider
            .anvil_set_next_block_timestamp(now)
            .await
            .context("evm_setNextBlockTimestamp failed")?;
        tmp_provider
            .evm_mine(None)
            .await
            .context("evm_mine failed")?;

        let address = format!("http://localhost:{port}");
        tracing::info!(
            port,
            http_url = address,
            ws_url,
            fork_block,
            "Forked anvil ready (timestamp synced to wall-clock)"
        );

        Ok(Self {
            address,
            _child: child,
            _port: locked_port,
        })
    }
}

impl Drop for ForkedAnvilL1 {
    fn drop(&mut self) {
        // Best-effort kill on drop so the process doesn't outlive the test on failure.
        let _ = self._child.start_kill();
    }
}

// ---------------------------------------------------------------------------
// ExternalServer  (old binary)
// ---------------------------------------------------------------------------

/// A running old-version server process with stdout/stderr piped to a log file.
pub struct ExternalServer {
    child: tokio::process::Child,
    pub rpc_ws_url: String,
    _port: LockedPort,
}

impl ExternalServer {
    /// Spawns the old server binary and waits until its TCP port accepts connections.
    pub async fn start(
        bin: &Path,
        db_path: &Path,
        genesis_json: &Path,
        config_yaml: &Path,
        anvil_url: &str,
        env_vars: &HashMap<String, String>,
        log_path: &Path,
    ) -> anyhow::Result<Self> {
        let locked_port = LockedPort::acquire_unused().await?;
        let port = locked_port.port;

        let log_file = File::create(log_path).context("failed to create server log")?;
        let log_file2 = log_file.try_clone()?;

        tracing::info!(
            bin        = %bin.display(),
            db         = %db_path.display(),
            genesis    = %genesis_json.display(),
            config     = %config_yaml.display(),
            anvil_url,
            port,
            log        = %log_path.display(),
            "Spawning old server"
        );
        tracing::debug!(
            "Old server env overrides: \
             general_l1_rpc_url={anvil_url}, \
             general_rocks_db_path={}, \
             rpc_address=0.0.0.0:{port}, \
             network_enabled=false, \
             fake_fri_provers=true, \
             fake_snark_provers=true, \
             fusaka_upgrade_timestamp=MAX",
            db_path.display(),
        );

        let child = Command::new(bin)
            .arg("--config")
            .arg(config_yaml)
            // Pass all secret env vars (smart_config picks up what it needs)
            .envs(env_vars)
            // Override runtime-specific settings
            .env("general_l1_rpc_url", anvil_url)
            .env("general_rocks_db_path", db_path)
            .env("genesis_genesis_input_path", genesis_json)
            .env("rpc_address", format!("0.0.0.0:{port}"))
            .env("network_enabled", "false")
            .env("batch_verification_server_enabled", "false")
            .env("batch_verification_client_enabled", "false")
            // Set fusaka timestamp far in future so it won't trigger during the test
            .env("l1_sender_fusaka_upgrade_timestamp", u64::MAX.to_string())
            // Use fake provers for fast batch sealing
            .env("prover_api_fake_fri_provers_enabled", "true")
            .env("prover_api_fake_fri_provers_compute_time", "200ms")
            .env("prover_api_fake_fri_provers_min_age", "0ms")
            .env("prover_api_fake_snark_provers_enabled", "true")
            .env("prover_api_fake_snark_provers_max_batch_age", "0ms")
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file2))
            .spawn()
            .with_context(|| format!("failed to spawn {}", bin.display()))?;

        let pid = child.id().unwrap_or(0);
        let rpc_ws_url = format!("ws://localhost:{port}");

        let server = Self {
            child,
            rpc_ws_url: rpc_ws_url.clone(),
            _port: locked_port,
        };

        tracing::info!(
            pid,
            port,
            rpc_ws_url,
            "Old server process started, waiting for TCP..."
        );
        let start = Instant::now();
        wait_for_tcp(port)
            .await
            .context("old server did not start in time")?;
        tracing::info!(
            pid,
            port,
            elapsed_ms = start.elapsed().as_millis(),
            "Old server is accepting connections"
        );

        Ok(server)
    }

    /// Sends SIGTERM and waits up to 60 s for graceful shutdown.
    pub async fn stop(mut self) -> anyhow::Result<()> {
        let pid = self
            .child
            .id()
            .context("old server process already exited")?;

        tracing::info!(pid, "Sending SIGTERM to old server...");

        let kill_result = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await;
        match kill_result {
            Ok(s) if s.success() => {}
            Ok(_) => {
                // Non-zero exit: process already exited — safe to proceed to wait().
                tracing::debug!(pid, "Old server already exited before SIGTERM was sent");
            }
            Err(e) => anyhow::bail!("failed to send SIGTERM: {e}"),
        }

        let start = Instant::now();
        let exit_status = tokio::time::timeout(Duration::from_secs(60), self.child.wait())
            .await
            .context("old server did not shut down within 60 s")?
            .context("error waiting for old server exit")?;

        tracing::info!(
            pid,
            exit_status = %exit_status,
            elapsed_ms = start.elapsed().as_millis(),
            "Old server stopped"
        );
        Ok(())
    }
}

impl Drop for ExternalServer {
    fn drop(&mut self) {
        // Best-effort kill on drop so the process doesn't outlive the test on failure
        // (e.g. if Phase 1 times out before `stop()` is called, which would otherwise
        // leave the old server holding DB files open).
        let _ = self.child.start_kill();
    }
}

// ---------------------------------------------------------------------------
// Helpers: DB management
// ---------------------------------------------------------------------------

/// Copies the pristine snapshot to the run-dir DB and removes the `batch` subdir.
/// The `batch` dir must not exist when starting a server from a snapshot.
pub async fn copy_db_for_run(pristine: &Path, dest: &Path) -> anyhow::Result<()> {
    tracing::info!(
        src = %pristine.display(),
        dst = %dest.display(),
        "Copying pristine DB to run directory (this may take a while for large DBs)..."
    );

    fs::create_dir_all(dest).context("failed to create run DB dir")?;

    let start = Instant::now();
    // `content_only(true)` copies the *contents* of `pristine` into `dest`,
    // matching the rsync trailing-slash behaviour we had before.
    let opts = fs_extra::dir::CopyOptions::new().content_only(true);
    fs_extra::dir::copy(pristine, dest, &opts)
        .context("failed to copy pristine DB to run directory")?;

    tracing::info!(
        dst = %dest.display(),
        elapsed_secs = start.elapsed().as_secs_f32(),
        "Pristine DB copied to run directory"
    );

    // Remove the batch dir — required before starting a server on a snapshot
    let batch_dir = dest.join("batch");
    if batch_dir.exists() {
        fs::remove_dir_all(&batch_dir).context("failed to remove batch dir")?;
        tracing::info!("Removed 'batch' subdir from run DB (required for snapshot start)");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers: Kubernetes API (kube-rs)
// ---------------------------------------------------------------------------

/// Creates a kube client from the default kubeconfig.
pub async fn kube_client() -> anyhow::Result<Client> {
    tracing::info!("Creating Kubernetes client from default kubeconfig...");
    let client = Client::try_default()
        .await
        .context("failed to create Kubernetes client (is KUBECONFIG set?)")?;
    tracing::info!("Kubernetes client ready");
    Ok(client)
}

/// Extracts the container image tag (e.g. `v0.19.0`) from the pod spec.
async fn get_pod_image_tag(kube: &Client, namespace: &str, pod: &str) -> anyhow::Result<String> {
    tracing::info!(
        namespace,
        pod,
        "Fetching pod spec to determine image tag..."
    );
    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);
    let pod_obj = pods
        .get(pod)
        .await
        .with_context(|| format!("failed to get pod '{pod}' in namespace '{namespace}'"))?;

    let spec = pod_obj
        .spec
        .as_ref()
        .with_context(|| format!("pod '{pod}' has no spec"))?;

    // Log all containers found so it's easy to see which one was picked.
    let container_names: Vec<&str> = spec.containers.iter().map(|c| c.name.as_str()).collect();
    tracing::debug!(namespace, pod, containers = ?container_names, "Pod containers found");

    // Prefer a container named "server" or containing "sequencer" to avoid picking
    // a sidecar (e.g. config-reloader) when the pod has multiple containers.
    let container = spec
        .containers
        .iter()
        .find(|c| c.name == "server" || c.name.contains("sequencer"))
        .or_else(|| spec.containers.first())
        .with_context(|| format!("pod '{pod}' has no containers"))?;

    let image = container
        .image
        .as_deref()
        .with_context(|| format!("pod '{pod}' container '{}' has no image", container.name))?;

    // Parse tag from e.g. "ghcr.io/matter-labs/zksync-os-server:v0.19.0"
    let tag = image
        .rsplit(':')
        .next()
        .with_context(|| format!("cannot parse image tag from: {image}"))?
        .trim()
        .to_string();

    anyhow::ensure!(!tag.is_empty(), "empty image tag");
    tracing::info!(
        image,
        tag,
        container = container.name,
        "Detected pod image tag (will be used for binary download + cache key)"
    );
    Ok(tag)
}

/// Runs the snapshot shell script inside the pod, then streams the resulting
/// directory out via `tar cf - /tmp/db-snapshot` and extracts it locally.
/// No intermediate file is created on the pod's disk for the transfer.
async fn download_db_snapshot(
    kube: &Client,
    namespace: &str,
    pod: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    // NOTE: The snapshot script captures SST files (immutable) before metadata,
    // which provides a best-effort consistent view of a live RocksDB instance.
    // This is NOT a true checkpoint (see RocksDB Checkpoint API). Gross
    // inconsistencies would cause Phase 1 to fail with a DB corruption error,
    // making test failures easy to diagnose.
    tracing::info!(namespace, pod, "Running DB snapshot script inside pod...");

    let pods: Api<Pod> = Api::namespaced(kube.clone(), namespace);

    // ── Step 1: run the snapshot script ──────────────────────────────────────
    // Capture stderr for diagnostics; drain it concurrently with status so the
    // DuplexStream buffer doesn't fill and deadlock the background I/O task.
    let script_start = Instant::now();
    let mut script_exec = pods
        .exec(
            pod,
            ["sh", "-c", POD_SNAPSHOT_SCRIPT],
            &AttachParams::default().stdin(false).stdout(false),
        )
        .await
        .context("failed to exec snapshot script in pod")?;

    let mut stderr = script_exec.stderr().context("exec has no stderr")?;
    let status_fut = script_exec
        .take_status()
        .expect("exec always has a status stream");

    let mut stderr_buf = String::new();
    let (_, status) = tokio::join!(stderr.read_to_string(&mut stderr_buf), status_fut,);
    let status = status.context("snapshot script returned no status")?;

    if !stderr_buf.is_empty() {
        tracing::debug!(
            script_stderr = stderr_buf.as_str(),
            "Snapshot script stderr output"
        );
    }

    anyhow::ensure!(
        status.status.as_deref() == Some("Success"),
        "snapshot script failed (status={:?}):\n{stderr_buf}",
        status.status,
    );

    tracing::info!(
        elapsed_secs = script_start.elapsed().as_secs_f32(),
        "DB snapshot script complete inside pod, streaming archive to local disk..."
    );

    // ── Step 2: stream `tar cf - /tmp/db-snapshot` to a local temp file ──────
    // Streaming avoids buffering the entire archive in RAM (production DBs can
    // be tens of GiB). Stderr is captured concurrently so tar warnings are logged
    // rather than silently discarded, and to prevent the DuplexStream buffer from
    // filling and deadlocking the background I/O task.
    let stream_start = Instant::now();
    let mut tar_exec = pods
        .exec(
            pod,
            ["tar", "cf", "-", "-C", REMOTE_SNAPSHOT_DIR, "."],
            &AttachParams::default().stdin(false).stderr(true),
        )
        .await
        .context("failed to exec tar stream in pod")?;

    let mut stdout = tar_exec.stdout().context("tar exec has no stdout")?;
    let mut stderr = tar_exec.stderr().context("tar exec has no stderr")?;
    let status_fut = tar_exec
        .take_status()
        .expect("exec always has a status stream");

    // Write stdout directly to a temp file.
    let tmp_archive = tempfile::NamedTempFile::new().context("failed to create temp archive")?;
    let mut async_out = tokio::fs::File::from_std(
        tmp_archive
            .as_file()
            .try_clone()
            .context("failed to clone temp archive fd")?,
    );

    let (copy_result, stderr_result, status) = tokio::join!(
        tokio::io::copy(&mut stdout, &mut async_out),
        async {
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).await.map(|_| buf)
        },
        status_fut,
    );

    let bytes_streamed = copy_result.context("failed to stream tar from pod")?;
    drop(async_out); // flush before reading back

    if let Ok(s) = &stderr_result
        && !s.is_empty()
    {
        tracing::debug!(tar_stderr = s.as_str(), "tar exec produced stderr output");
    }

    let status = status.context("tar exec returned no status")?;
    anyhow::ensure!(
        status.status.as_deref() == Some("Success"),
        "tar exec failed (status={:?})",
        status.status,
    );

    tracing::info!(
        archive_bytes = bytes_streamed,
        archive_mb = bytes_streamed / 1_048_576,
        elapsed_secs = stream_start.elapsed().as_secs_f32(),
        "Tar archive streamed from pod, extracting to local disk..."
    );

    // ── Step 3: extract from temp file ───────────────────────────────────────
    // `reopen()` gives a fresh OS file handle positioned at byte 0.
    let extract_start = Instant::now();
    fs::create_dir_all(dest).context("failed to create DB dest dir")?;
    let reader = tmp_archive
        .reopen()
        .context("failed to reopen temp archive")?;
    let mut archive = tar::Archive::new(reader);
    archive
        .unpack(dest)
        .context("failed to unpack DB snapshot")?;

    tracing::info!(
        dest = %dest.display(),
        elapsed_secs = extract_start.elapsed().as_secs_f32(),
        "Archive extracted, verifying RocksDB dirs..."
    );

    // Verify expected RocksDB dirs are present (fri_proofs is alongside node1/)
    for rel in &[
        "fri_proofs",
        "node1/block_replay_wal",
        "node1/tree",
        "node1/repository",
        "node1/state_full_diffs",
        "node1/preimages_full_diffs",
    ] {
        let dir = dest.join(rel);
        anyhow::ensure!(dir.exists(), "missing expected DB dir after extract: {rel}");

        let entries: Vec<_> = fs::read_dir(&dir)
            .with_context(|| format!("cannot read {}", dir.display()))?
            .filter_map(|e| e.ok())
            .collect();

        let sst_count = entries
            .iter()
            .filter(|e| e.path().extension().map(|x| x == "sst").unwrap_or(false))
            .count();
        let has_sst = sst_count > 0;
        anyhow::ensure!(has_sst, "no SST files in {rel} after extract");

        tracing::debug!(
            rel,
            sst_files = sst_count,
            total_files = entries.len(),
            "Verified RocksDB dir"
        );
    }

    tracing::info!(
        dest = %dest.display(),
        total_elapsed_secs = script_start.elapsed().as_secs_f32(),
        "DB snapshot ready"
    );
    Ok(())
}

/// Fetches a single key from a ConfigMap and writes it to `dest`.
async fn download_configmap_key(
    kube: &Client,
    namespace: &str,
    configmap: &str,
    key: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    tracing::info!(namespace, configmap, key, "Fetching ConfigMap entry...");
    let cms: Api<ConfigMap> = Api::namespaced(kube.clone(), namespace);
    let cm = cms
        .get(configmap)
        .await
        .with_context(|| format!("failed to get configmap '{configmap}'"))?;

    let value = cm
        .data
        .as_ref()
        .and_then(|d| d.get(key))
        .with_context(|| format!("configmap '{configmap}' has no key '{key}'"))?;

    fs::write(dest, value).with_context(|| format!("failed to write {}", dest.display()))?;

    tracing::info!(
        configmap,
        key,
        dest    = %dest.display(),
        bytes   = value.len(),
        "ConfigMap entry downloaded"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers: GitHub binary download
// ---------------------------------------------------------------------------

/// Downloads the old server binary from GitHub releases and makes it executable.
async fn download_old_binary(image_tag: &str, dest: &Path) -> anyhow::Result<()> {
    let target = local_target_triple();
    let archive_name = format!("zksync-os-server-{image_tag}-{target}.tar.gz");
    let url = format!(
        "https://github.com/matter-labs/zksync-os-server/releases/download/{image_tag}/{archive_name}"
    );

    tracing::info!(
        url,
        target,
        "Downloading old server binary from GitHub releases..."
    );

    let tmp_archive = tempfile::NamedTempFile::new().context("failed to create temp archive")?;

    {
        use tokio::io::AsyncWriteExt as _;
        let dl_start = Instant::now();
        let mut response = crate::download_prover_binary(&url)
            .await
            .with_context(|| format!("failed to download old server binary from {url}"))?;

        if let Some(len) = response.content_length() {
            tracing::info!(
                size_mb = len / 1_048_576,
                "Content-Length known, downloading..."
            );
        }

        let mut async_file = tokio::fs::File::from_std(
            tmp_archive
                .as_file()
                .try_clone()
                .context("failed to clone temp archive fd")?,
        );
        let mut bytes_downloaded: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .context("error reading response chunk")?
        {
            bytes_downloaded += chunk.len() as u64;
            async_file
                .write_all(&chunk)
                .await
                .context("failed to write chunk to temp archive")?;
        }
        async_file
            .flush()
            .await
            .context("failed to flush temp archive")?;

        tracing::info!(
            bytes_downloaded,
            mb_downloaded = bytes_downloaded / 1_048_576,
            elapsed_secs = dl_start.elapsed().as_secs_f32(),
            "Binary archive downloaded, extracting..."
        );
    }

    // Extract .tar.gz — no subprocess needed
    let extract_start = Instant::now();
    let archive_file = File::open(tmp_archive.path()).context("failed to open archive")?;
    let gz = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(gz);

    let tmp_dir = tempfile::tempdir().context("failed to create extraction tempdir")?;
    archive
        .unpack(tmp_dir.path())
        .context("failed to extract binary archive")?;

    let candidates = [
        tmp_dir.path().join("zksync-os-server"),
        tmp_dir
            .path()
            .join(format!("zksync-os-server-{image_tag}-{target}")),
    ];
    let binary = candidates
        .iter()
        .find(|p| p.exists())
        .with_context(|| format!("binary not found inside archive; tried: {candidates:?}"))?;

    tracing::debug!(binary = %binary.display(), dest = %dest.display(), "Found binary inside archive, copying to cache");

    fs::copy(binary, dest)
        .with_context(|| format!("failed to copy binary to {}", dest.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dest)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(dest, perms)?;
    }

    tracing::info!(
        dest          = %dest.display(),
        elapsed_secs  = extract_start.elapsed().as_secs_f32(),
        "Old binary ready"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers: networking
// ---------------------------------------------------------------------------

/// Polls a TCP port until it accepts connections (up to ~30 s).
async fn wait_for_tcp(port: u16) -> anyhow::Result<()> {
    (|| async {
        tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .map(|_| ())
            .context("port not ready")
    })
    .retry(
        ConstantBuilder::default()
            .with_delay(Duration::from_millis(300))
            .with_max_times(100),
    )
    .notify(|_: &anyhow::Error, _| {
        tracing::debug!(port, "Waiting for TCP port...");
    })
    .await
    .with_context(|| format!("port {port} did not become ready"))
}

// ---------------------------------------------------------------------------
// Helpers: platform
// ---------------------------------------------------------------------------

fn local_target_triple() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", _) => "universal-apple-darwin",
        (os, arch) => panic!("unsupported platform: {os}-{arch}"),
    }
}
