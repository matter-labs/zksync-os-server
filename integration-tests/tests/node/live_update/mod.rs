//! Live-DB upgrade test: verifies that the new server can continue from a DB
//! produced by the previous released version running against a live cluster snapshot.
//!
//! ## How to run
//!
//! ```bash
//! LIVE_UPDATE_NAMESPACE=testnet-alpha \
//! LIVE_UPDATE_POD=sequencer-c-0 \
//! LIVE_UPDATE_L1_RPC_URL=https://sepolia.infura.io/v3/<key> \
//! cargo nextest run -p zksync_os_integration_tests \
//!   --features live-update \
//!   --run-ignored only \
//!   node::live_update
//! ```
//!
//! All artifacts (DB snapshot, genesis.json, config.yaml, old binary) are cached at
//! `<workspace>/live-update-cache/<namespace>/<pod>/` and reused on subsequent runs unless the
//! pod's container image tag changes.  Per-run logs (anvil, old-server) are saved to
//! `live-update-cache/<namespace>/<pod>/runs/<timestamp>/logs/`.
//!
//! ## Optional env vars
//! - `LIVE_UPDATE_ARTIFACTS_DIR`  – override cache location
//! - `LIVE_UPDATE_OLD_BIN`        – skip GitHub download, use this binary
//! - `LIVE_UPDATE_L2_WALLET_SK`   – hex private key of a funded L2 account for test transactions

use std::time::{Duration, Instant};

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::WalletProvider;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::Context;
use backon::{ConstantBuilder, Retryable};
use reth_tasks::{PanickedTaskError, Runtime, RuntimeBuilder, RuntimeConfig};
use smart_config::{ConfigRepository, ConfigSources, Environment};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tracing::Instrument;
use zksync_os_integration_tests::live_update::{
    ExternalServer, ForkedAnvilL1, LiveUpdateArtifacts, LiveUpdateConfig, LiveUpdateRunDir,
    OperatorKeys, copy_db_for_run, kube_client,
};
use zksync_os_integration_tests::{LockedPort, NODE_SHUTDOWN_TIMEOUT};
use zksync_os_internal_config::InternalConfigManager;
use zksync_os_server::INTERNAL_CONFIG_FILE_NAME;
use zksync_os_server::config::{
    BatchVerificationConfig, FakeFriProversConfig, FakeSnarkProversConfig, RebuildBlocksConfig,
    build_external_config, load_config_file_sources,
};
use zksync_os_state_full_diffs::FullDiffsState;

// ---------------------------------------------------------------------------
// Test entry point
// ---------------------------------------------------------------------------

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires LIVE_UPDATE_NAMESPACE, LIVE_UPDATE_POD, LIVE_UPDATE_L1_RPC_URL env vars and --features live-update; run with --include-ignored"]
async fn live_update() -> anyhow::Result<()> {
    let config = match LiveUpdateConfig::from_env() {
        Some(c) => c,
        None => {
            tracing::warn!(
                "Skipping live_update: set LIVE_UPDATE_NAMESPACE, LIVE_UPDATE_POD, \
                 and LIVE_UPDATE_L1_RPC_URL to run this test"
            );
            return Ok(());
        }
    };

    let test_start = Instant::now();

    // ── 1. Build a shared kube client ───────────────────────────────────────
    tracing::info!("=== Step 1/6: Creating Kubernetes client ===");
    let kube = kube_client()
        .await
        .context("failed to create Kubernetes client")?;

    // ── 2. Cache artifacts ──────────────────────────────────────────────────
    tracing::info!("=== Step 2/6: Ensuring artifacts (DB snapshot, config, old binary) ===");
    let artifacts = LiveUpdateArtifacts::ensure(&config, &kube)
        .await
        .context("failed to ensure live-update artifacts")?;

    // ── 3. Fetch operator keys (never cached) ────────────────────────────────
    tracing::info!("=== Step 3/6: Fetching operator keys ===");
    let operator_keys = OperatorKeys::fetch(&kube, &config.namespace)
        .await
        .context("failed to fetch operator keys from k8s secret")?;

    // ── 4. Create per-run directory ──────────────────────────────────────────
    tracing::info!("=== Step 4/6: Creating run directory ===");
    let run_dir = LiveUpdateRunDir::create(&artifacts.dir)?;
    tracing::info!(
        run_dir  = %run_dir.dir.display(),
        "Run directory ready. Logs will be written to {}/logs/",
        run_dir.dir.display()
    );

    // ── 5. Copy pristine DB to run dir (and delete `batch` subdir) ───────────
    tracing::info!("=== Step 5/6: Copying pristine DB to run directory ===");
    copy_db_for_run(&artifacts.pristine_db, &run_dir.db)
        .await
        .context("failed to copy DB")?;

    // ── 6. Start forked Anvil L1 ─────────────────────────────────────────────
    tracing::info!("=== Step 6/6: Starting forked Anvil L1 ===");
    let anvil = ForkedAnvilL1::start(
        &config.l1_rpc_url,
        artifacts.l1_fork_block,
        &run_dir.log_path("anvil"),
    )
    .await
    .context("failed to start forked Anvil")?;

    // ── Phase 1: old binary ──────────────────────────────────────────────────
    tracing::info!(
        tag     = artifacts.image_tag,
        bin     = %artifacts.old_bin.display(),
        log     = %run_dir.log_path("old-server").display(),
        "====== Phase 1: old server ({}) ======",
        artifacts.image_tag,
    );

    let old_server = ExternalServer::start(
        &artifacts.old_bin,
        &run_dir.db,
        &artifacts.genesis_json,
        &artifacts.config_yaml,
        &anvil.address,
        &operator_keys.all,
        &run_dir.log_path("old-server"),
    )
    .await
    .context("failed to start old server")?;

    // Optionally send test transactions to drive batch sealing faster
    if let Some(wallet_sk) = &config.l2_wallet_sk {
        tracing::info!("Sending test transactions to accelerate batch sealing on old server...");
        send_test_transactions(&old_server.rpc_ws_url, wallet_sk, 5)
            .await
            .context("failed to send test transactions on old server")?;
    }

    tracing::info!("Waiting for old server to produce 3 new L2 blocks (timeout: 5 min)...");
    wait_for_new_l2_blocks(&old_server.rpc_ws_url, 3, Duration::from_secs(300))
        .await
        .context("old server did not produce L2 blocks — check old-server.log")?;

    tracing::info!("Phase 1 complete: old server produced new L2 blocks");
    old_server
        .stop()
        .await
        .context("failed to stop old server")?;

    // ── Phase 2: new server (in-process, current build) ──────────────────────
    tracing::info!(
        log = %run_dir.log_path("new-server").display(),
        "====== Phase 2: new server (current build) ======",
    );

    let new_server =
        NewInProcessServer::start(&artifacts, &run_dir.db, &anvil.address, &operator_keys.all)
            .await
            .context("failed to start new in-process server")?;

    if let Some(wallet_sk) = &config.l2_wallet_sk {
        tracing::info!("Sending test transactions to accelerate batch sealing on new server...");
        send_test_transactions(&new_server.rpc_ws_url, wallet_sk, 5)
            .await
            .context("failed to send test transactions on new server")?;
    }

    tracing::info!("Waiting for new server to produce 3 new L2 blocks (timeout: 5 min)...");
    wait_for_new_l2_blocks(&new_server.rpc_ws_url, 3, Duration::from_secs(300))
        .await
        .context("new server did not produce L2 blocks after upgrade — check test output")?;

    tracing::info!("Phase 2 complete: new server produced new L2 blocks after upgrade");
    new_server
        .stop()
        .await
        .context("failed to stop new server")?;

    tracing::info!(
        total_elapsed_secs = test_start.elapsed().as_secs_f32(),
        "====== live_update test PASSED ======"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// New in-process server
// ---------------------------------------------------------------------------

struct NewInProcessServer {
    runtime: Runtime,
    task_manager_handle: Option<JoinHandle<Result<(), PanickedTaskError>>>,
    pub rpc_ws_url: String,
    _port: LockedPort,
}

impl NewInProcessServer {
    async fn start(
        artifacts: &LiveUpdateArtifacts,
        db_path: &std::path::Path,
        anvil_url: &str,
        env_vars: &std::collections::HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let locked_port = LockedPort::acquire_unused().await?;
        let port = locked_port.port;
        let rpc_ws_url = format!("ws://localhost:{port}");

        tracing::info!(
            config   = %artifacts.config_yaml.display(),
            genesis  = %artifacts.genesis_json.display(),
            db       = %db_path.display(),
            anvil    = anvil_url,
            port,
            "Building new server config from cluster config + overrides"
        );

        let config_schema = zksync_os_server::config::Config::schema();
        let mut config_sources = ConfigSources::default();
        load_config_file_sources(&mut config_sources, &[artifacts.config_yaml.clone()]);

        // Inject operator keys without touching the process environment.
        // `Environment::from_iter` builds a config source directly from the HashMap,
        // avoiding the UB of `std::env::set_var` on a live multi-threaded runtime.
        let operator_env = Environment::from_iter("", env_vars.iter().map(|(k, v)| (k, v)));
        config_sources.push(operator_env);

        // Process environment for any additional runtime overrides.
        let mut env = Environment::prefixed("");
        env.coerce_json()
            .expect("failed to configure env JSON coercion");
        config_sources.push(env);

        let config_repo = ConfigRepository::new(&config_schema).with_all(config_sources);
        let mut config = build_external_config(config_repo).await;

        // Override all runtime-specific settings — log each one for easy debugging.
        config.general_config.l1_rpc_url = anvil_url.to_string();
        // The snapshot layout is <run_dir>/db/node1/{tree,repository,...} so
        // rocks_db_path must point to the node1 subdirectory, not the DB root.
        config.general_config.rocks_db_path = db_path.join("node1");

        // Replicate what production main() does: read internal_config.json from the DB
        // root and merge signer blacklist + optional failing-block rebuild config.
        let internal_config =
            InternalConfigManager::new(db_path.join("node1").join(INTERNAL_CONFIG_FILE_NAME))
                .context("failed to create InternalConfigManager")?
                .read_config()
                .context("failed to read internal config")?;
        tracing::info!(?internal_config, "Loaded internal config from DB snapshot");
        config
            .rpc_config
            .l2_signer_blacklist
            .extend(internal_config.l2_signer_blacklist);
        if let Some(failing_block) = internal_config.failing_block {
            anyhow::ensure!(
                config.sequencer_config.block_rebuild.is_none(),
                "external config specifies block_rebuild and internal config specifies \
                 failing_block; remove one to avoid conflicts"
            );
            config.sequencer_config.block_rebuild = Some(RebuildBlocksConfig {
                from_block: failing_block,
                blocks_to_empty: vec![failing_block],
                reset_timestamps: false,
            });
        }

        config.genesis_config.genesis_input_path = Some(artifacts.genesis_json.clone());
        config.rpc_config.address = format!("0.0.0.0:{port}");
        config.network_config.enabled = false;
        // Set fusaka timestamp far in the future so it won't trigger during the test.
        // The live cluster config may have a past or near-future timestamp.
        config.l1_sender_config.fusaka_upgrade_timestamp = u64::MAX;
        config.batch_verification_config = BatchVerificationConfig {
            server_enabled: false,
            client_enabled: false,
            ..config.batch_verification_config
        };
        // Use fake provers so batches seal quickly
        config.prover_api_config.fake_fri_provers = FakeFriProversConfig {
            enabled: true,
            compute_time: Duration::from_millis(200),
            min_age: Duration::ZERO,
            ..config.prover_api_config.fake_fri_provers
        };
        config.prover_api_config.fake_snark_provers = FakeSnarkProversConfig {
            enabled: true,
            max_batch_age: Duration::ZERO,
            ..config.prover_api_config.fake_snark_provers
        };
        // fri_proofs lives inside the DB root alongside node1/, consistent with the
        // snapshot layout and the path used by the old server in Phase 1.
        config.prover_api_config.proof_storage.path = db_path.join("fri_proofs");

        tracing::debug!(
            "New server config overrides applied: \
             l1_rpc_url={anvil_url}, \
             db={}, \
             rpc_address=0.0.0.0:{port}, \
             network=false, \
             batch_verification=false, \
             fake_fri_provers=true, \
             fake_snark_provers=true, \
             fusaka_upgrade_timestamp=MAX",
            db_path.display(),
        );

        let runtime = RuntimeBuilder::new(RuntimeConfig::with_existing_handle(Handle::current()))
            .build()
            .expect("failed to build reth runtime");

        tracing::info!(port, "Starting new in-process server...");
        let start = Instant::now();
        let node_span = tracing::info_span!("new_server");
        zksync_os_server::run::<FullDiffsState>(&runtime, config)
            .instrument(node_span)
            .await;
        let task_manager_handle = runtime
            .take_task_manager_handle()
            .expect("Runtime must contain a TaskManager handle after run()");

        // A TCP-only check is not enough: the WebSocket handshake may fail while the
        // server is still initialising. Match the pattern from ForkedAnvilL1::start().
        let tmp_provider = (|| async {
            ProviderBuilder::new()
                .connect(&rpc_ws_url)
                .await
                .context("failed to connect to new server")
        })
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_millis(200))
                .with_max_times(50),
        )
        .notify(|err: &anyhow::Error, dur: Duration| {
            tracing::debug!(%err, ?dur, "retrying connection to new server");
        })
        .await
        .context("new server did not start in time")?;
        tmp_provider
            .get_block_number()
            .await
            .context("new server did not respond to get_block_number")?;

        tracing::info!(
            port,
            rpc_ws_url,
            elapsed_ms = start.elapsed().as_millis(),
            "New server is accepting connections"
        );

        Ok(Self {
            runtime,
            task_manager_handle: Some(task_manager_handle),
            rpc_ws_url,
            _port: locked_port,
        })
    }

    async fn stop(mut self) -> anyhow::Result<()> {
        tracing::info!("Shutting down new server (timeout: {NODE_SHUTDOWN_TIMEOUT:?})...");
        let start = Instant::now();
        if !self
            .runtime
            .graceful_shutdown_with_timeout(NODE_SHUTDOWN_TIMEOUT)
        {
            anyhow::bail!("new server failed to shut down within {NODE_SHUTDOWN_TIMEOUT:?}");
        }
        // Surface any critical task crash that occurred during the test run.
        let handle = self
            .task_manager_handle
            .take()
            .expect("task_manager_handle already consumed");
        match handle.await {
            Ok(Err(e)) => anyhow::bail!("new server critical task crashed: {e}"),
            Ok(Ok(())) => {}
            Err(e) => anyhow::bail!("task manager join failed: {e}"),
        }
        tracing::info!(
            elapsed_ms = start.elapsed().as_millis(),
            "New server stopped gracefully"
        );
        Ok(())
    }
}

impl Drop for NewInProcessServer {
    fn drop(&mut self) {
        // Best-effort shutdown if stop() was not called (e.g. on test panic or timeout).
        self.runtime
            .graceful_shutdown_with_timeout(Duration::from_secs(5));
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Waits until at least `n` new L2 blocks have been produced.
///
/// The entire operation — including initial connection retries and waiting for the
/// RPC to finish initialising — is bounded by `max_wait`.
async fn wait_for_new_l2_blocks(ws_url: &str, n: u64, max_wait: Duration) -> anyhow::Result<()> {
    tokio::time::timeout(max_wait, async {
        // Retry connecting until the server is reachable.
        // No inner retry cap: the outer tokio::time::timeout(max_wait) is the sole bound.
        // A fixed with_max_times (e.g. 50 × 500 ms = 25 s) could exhaust before max_wait
        // and produce a misleading error when the server would have connected at second 30.
        let provider = (|| ProviderBuilder::new().connect(ws_url))
            .retry(
                ConstantBuilder::default()
                    .with_delay(Duration::from_millis(500))
                    .with_max_times(usize::MAX),
            )
            .notify(|err, _| {
                tracing::debug!(%err, ws_url, "Retrying L2 provider connection...");
            })
            .await
            .with_context(|| format!("failed to connect to L2 at {ws_url}"))?;

        // Retry the baseline call until the RPC is fully initialised.
        let start_block = (|| async { provider.get_block_number().await })
            .retry(
                ConstantBuilder::default()
                    .with_delay(Duration::from_millis(500))
                    .with_max_times(usize::MAX),
            )
            .notify(|err, _| {
                tracing::debug!(%err, "Retrying get_block_number (RPC not yet initialised)...");
            })
            .await
            .context("failed to get baseline block number from L2")?;

        let target_block = start_block + n;
        tracing::info!(start_block, target_block, "Polling for new L2 blocks...");

        let poll_start = Instant::now();
        let mut last_progress_log = Instant::now();
        let progress_interval = Duration::from_secs(30);
        let mut last_known_block = start_block;

        loop {
            let current = match provider.get_block_number().await {
                Ok(b) => {
                    last_known_block = b;
                    b
                }
                Err(e) => {
                    tracing::debug!(err = %e, "get_block_number failed, will retry");
                    last_known_block
                }
            };

            if current >= target_block {
                tracing::info!(
                    current_block = current,
                    start_block,
                    target_block,
                    elapsed_secs = poll_start.elapsed().as_secs_f32(),
                    "Target block reached — {n} new L2 blocks produced"
                );
                return anyhow::Ok(());
            }

            // Log progress every 30 s so it's easy to see the test isn't hung.
            if last_progress_log.elapsed() >= progress_interval {
                tracing::info!(
                    current_block = current,
                    start_block,
                    target_block,
                    blocks_to_go = target_block.saturating_sub(current),
                    elapsed_secs = poll_start.elapsed().as_secs_f32(),
                    "Still waiting for L2 blocks..."
                );
                last_progress_log = Instant::now();
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .context("timed out waiting for new L2 blocks")?
}

/// Sends `count` simple ETH-transfer transactions from `wallet_sk` on the given L2.
/// Logs a warning and returns `Ok` if the wallet has no funds (non-fatal).
async fn send_test_transactions(ws_url: &str, wallet_sk: &str, count: u64) -> anyhow::Result<()> {
    use alloy::providers::ProviderBuilder;
    use std::str::FromStr;

    let signer =
        PrivateKeySigner::from_str(wallet_sk).context("invalid LIVE_UPDATE_L2_WALLET_SK")?;
    let address = signer.address();
    let wallet = alloy::network::EthereumWallet::new(signer);

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect(ws_url)
        .await
        .context("failed to connect L2 provider for test transactions")?;

    let balance = provider
        .get_balance(provider.default_signer_address())
        .await
        .unwrap_or(U256::ZERO);

    tracing::info!(
        address = %address,
        balance_wei = %balance,
        "Wallet balance on L2"
    );

    if balance.is_zero() {
        tracing::warn!(
            address = %address,
            "Wallet has zero balance on L2 — skipping test transaction sending"
        );
        return Ok(());
    }

    tracing::info!(address = %address, count, "Sending test transactions on L2...");
    let mut sent = 0u64;
    for i in 0..count {
        let tx = TransactionRequest::default()
            .with_to(Address::random())
            .with_value(U256::from(1u64));
        match provider.send_transaction(tx).await {
            Ok(pending) => {
                sent += 1;
                tracing::debug!(i, tx_hash = %pending.tx_hash(), "Test transaction sent");
            }
            Err(e) => {
                tracing::warn!(i, err = %e, "Failed to send test transaction (non-fatal)");
            }
        }
    }

    tracing::info!(sent, total = count, "Test transactions submitted");
    Ok(())
}
