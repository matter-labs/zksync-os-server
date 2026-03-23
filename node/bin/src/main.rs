use clap::{Parser, Subcommand};
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig};
use smart_config::{ConfigRepository, ConfigSources, Environment};
use std::sync::mpsc;
use std::{path::Path, path::PathBuf, str::FromStr, time::Duration};
use tempfile::TempDir;
use tokio::runtime::Handle;
use tokio::signal::unix::{SignalKind, signal};
use zksync_os_internal_config::InternalConfigManager;
use zksync_os_metadata::NODE_VERSION;
use zksync_os_observability::prometheus::PrometheusExporterConfig;
use zksync_os_server::config::{
    Config, ConfigArgs, ProofStorageConfig, RebuildBlocksConfig, StateBackendConfig,
    build_external_config, load_config_file_sources,
};
use zksync_os_server::default_protocol_version::{DEFAULT_ROCKS_DB_PATH, PROTOCOL_VERSION};
use zksync_os_server::{INTERNAL_CONFIG_FILE_NAME, run};
use zksync_os_state::StateHandle;
use zksync_os_state_full_diffs::FullDiffsState;

const IMMEDIATE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Configuration-related tools.
    Config(ConfigArgs),
}

#[derive(Debug, Parser)]
#[command(author = "Matter Labs", version, about = "ZKsync OS node", long_about = None)]
struct Cli {
    /// Paths to JSON or YAML config files. Multiple files can be specified by repeating the flag
    /// (e.g. `--config main.yaml --config overrides.json`) or using `:` as a delimiter
    /// (e.g. `--config main.yaml:overrides.json`). Files are loaded in order, with later files
    /// taking precedence. If not specified, default config will be attempted to be loaded to fill
    /// in the config values for local setup. If default config is missing, no configs will be
    /// loaded, and they must be explicitly set via other configuration means (e.g. environment
    /// variables). Env variables override config settings from all files. The file format is
    /// detected based on the file extension (.json, .yaml, or .yml).
    #[arg(long, value_delimiter = ':')]
    config: Option<Vec<String>>,

    #[command(subcommand)]
    cmd: Option<CliCommand>,
}

fn load_config_defaults(config_sources: &mut ConfigSources, config_paths: Option<Vec<String>>) {
    // Process the config files if provided or if default exists
    let config_paths: Vec<PathBuf> = config_paths
        .filter(|paths| !paths.is_empty())
        .unwrap_or_else(|| {
            let shared_path = "./local-chains/local_dev.yaml".to_string();
            let default_path = format!("./local-chains/{PROTOCOL_VERSION}/default/config.yaml");
            let mut paths = vec![];
            if Path::new(&shared_path).exists() {
                paths.push(shared_path);
            }
            if Path::new(&default_path).exists() {
                paths.push(default_path);
            }
            paths
        })
        .into_iter()
        .map(|path| {
            PathBuf::from_str(&path).unwrap_or_else(|_| panic!("Invalid config file path: {path}"))
        })
        .collect();

    load_config_file_sources(config_sources, &config_paths);
}

#[tokio::main]
pub async fn main() {
    // Explicitly select the `ring` TLS crypto provider for rustls.
    //
    // Our dependency tree pulls in both `ring` and `aws-lc-rs` as rustls crypto backends
    // (via reqwest, gcp_auth, and other crates). When both are present, rustls cannot
    // auto-detect which one to use and panics on the first TLS connection with:
    //   "no process-level CryptoProvider is set"
    //
    // This must be called before any TLS connection is made (e.g. GCP KMS signing via HTTPS).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring crypto provider");

    let runtime = RuntimeBuilder::new(RuntimeConfig::with_existing_handle(Handle::current()))
        .build()
        .expect("failed to build runtime");

    let opt = Cli::parse();

    // =========== load configs ===========
    let config_schema = Config::schema();
    let mut config_sources = ConfigSources::default();

    // Process the config file if provided or if default exists
    load_config_defaults(&mut config_sources, opt.config);

    let mut env = Environment::prefixed("");
    // Enables JSON coercion - env variables with `__JSON` suffix can be used to force value
    // deserialization as JSON instead of plain string. This is useful to distinguish between "null"
    // and `null` (missing value). Usage example: `GENESIS_BRIDGEHUB_ADDRESS__JSON=null`
    env.coerce_json()
        .expect("failed to coerce JSON envvar values");
    config_sources.push(env);

    // =========== init observability ===========
    let observability_config =
        Config::observability(config_sources.clone()).expect("failed parsing observability config");
    let logs = zksync_os_observability::Logs::new(
        observability_config.log.format,
        observability_config.log.use_color,
    );
    let sentry = observability_config
        .sentry
        .dsn_url
        .clone()
        .map(|sentry_url| {
            zksync_os_observability::Sentry::new(&sentry_url)
                .expect("Failed to create Sentry config")
                .with_node_version(Some(NODE_VERSION.to_string()))
                .with_environment(observability_config.sentry.environment.clone())
        });
    let otlp = zksync_os_observability::OpenTelemetry::new(
        observability_config.otlp.level,
        observability_config.otlp.tracing_endpoint.clone(),
        observability_config.otlp.logging_endpoint.clone(),
    )
    .expect("Failed to create OpenTelemetry config");

    let _observability_guard = zksync_os_observability::ObservabilityBuilder::new()
        .with_logs(Some(logs))
        .with_sentry(sentry)
        .with_opentelemetry(Some(otlp))
        .build();

    let config_repo = ConfigRepository::new(&config_schema).with_all(config_sources);

    // =========== handle the CLI subcommand if any ===========
    if let Some(cmd) = opt.cmd {
        match cmd {
            CliCommand::Config(args) => {
                args.run(config_repo, "").unwrap();
                return;
            }
        }
    }

    let mut config = build_external_config(config_repo).await;
    tracing::info!(?config, "Loaded config");
    load_internal_config(&mut config);
    // ======= Run tasks ===========
    let ephemeral_enabled = config.general_config.ephemeral;
    if !ephemeral_enabled && config.general_config.ephemeral_state.is_some() {
        panic!("`ephemeral_state` requires `ephemeral` mode to be enabled");
    }
    let _ephemeral_guard = ephemeral_enabled.then(|| enable_ephemeral_mode(&mut config));
    let prometheus_port = config.observability_config.prometheus.port;

    match config.general_config.state_backend {
        StateBackendConfig::FullDiffs => run::<FullDiffsState>(&runtime, config).await,
        StateBackendConfig::Compacted => run::<StateHandle>(&runtime, config).await,
    };

    runtime.spawn_critical_with_graceful_shutdown_signal("prometheus", |shutdown| async move {
        if ephemeral_enabled {
            tracing::info!("Ephemeral mode enabled, skipping Prometheus exporter");
        } else {
            let prometheus: PrometheusExporterConfig =
                PrometheusExporterConfig::pull(prometheus_port);
            prometheus.run(shutdown).await.expect("prometheus failed");
        }
    });

    let task_manager_handle = runtime
        .take_task_manager_handle()
        .expect("Runtime must contain a TaskManager handle");

    tokio::select! {
        task_manager_result = task_manager_handle => {
            if let Ok(Err(err)) = task_manager_result {
                tracing::error!("shutting down due to error");
                eprintln!("Error: {err:?}");
                std::process::exit(1);
            }
        },
        _ = handle_delayed_termination(runtime) => {},
    }
}

async fn handle_delayed_termination(runtime: Runtime) {
    // sigint is sent on Ctrl+C
    let mut sigint =
        signal(SignalKind::interrupt()).expect("failed to register interrupt signal handler");

    // sigterm is sent on `kill <pid>` or by kubernetes during pod shutdown
    let mut sigterm =
        signal(SignalKind::terminate()).expect("failed to register terminate signal handler");
    tokio::select! {
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM: shutting down immediately");
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("rt-shutdown".to_string())
                .spawn(move || {
                    drop(runtime);
                    let _ = tx.send(());
                })
                .unwrap();

            let _ = rx.recv_timeout(IMMEDIATE_SHUTDOWN_TIMEOUT).inspect_err(|err| {
                tracing::warn!(%err, "runtime shutdown timed out");
            });
        },
        _ = sigint.recv() => {
            tracing::info!("received SIGINT: shutting down gracefully (within 10s)");

            runtime.graceful_shutdown_with_timeout(GRACEFUL_SHUTDOWN_TIMEOUT);
        },
    }
}

fn enable_ephemeral_mode(config: &mut Config) -> Option<TempDir> {
    let original_path = config.general_config.rocks_db_path.clone();
    if original_path != Path::new(DEFAULT_ROCKS_DB_PATH) {
        tracing::warn!(
            original_path = %original_path.display(),
            "general_rocks_db_path parameter is ignored in ephemeral mode"
        );
    }

    let tempdir = tempfile::tempdir()
        .expect("Failed to create temporary RocksDB directory for ephemeral mode");
    let tempdir_path = tempdir.path();
    tracing::info!(
        path = %tempdir_path.display(),
        "Ephemeral mode enabled. Using temporary directory for RocksDB and proof storage"
    );

    // Update config to use temporary directory
    config.general_config.rocks_db_path = tempdir_path.join("node");
    config.prover_api_config.proof_storage = ProofStorageConfig {
        path: tempdir_path.join("fri_proofs"),
        ..ProofStorageConfig::default()
    };

    // Disable services that are not needed in ephemeral mode
    config.prover_api_config.enabled = false;
    config.status_server_config.enabled = false;
    // todo: consider force-disabling
    // config.network_config.enabled = false;

    if let Some(ephemeral_state) = &config.general_config.ephemeral_state {
        tracing::info!("Loading ephemeral state from {}", ephemeral_state.display());
        zksync_os_server::util::unpack_ephemeral_state(
            ephemeral_state,
            &config.general_config.rocks_db_path,
        );
    }

    Some(tempdir)
}

fn load_internal_config(config: &mut Config) {
    let file_path = config
        .general_config
        .rocks_db_path
        .join(INTERNAL_CONFIG_FILE_NAME);
    let internal_config_manager =
        InternalConfigManager::new(file_path).expect("Failed to create internal config manager");
    let internal_config = internal_config_manager
        .read_config()
        .expect("Failed to read internal config");
    tracing::info!(?internal_config, "Loaded internal config");

    // Merging configs.
    config
        .rpc_config
        .l2_signer_blacklist
        .extend(internal_config.l2_signer_blacklist);
    if let Some(failing_block) = internal_config.failing_block {
        if config.sequencer_config.block_rebuild.is_some() {
            panic!(
                "External config specifies block rebuild: {:?} and internal config specifies failing block: {}. \
                 Please remove one of these settings to avoid conflicts.",
                config.sequencer_config.block_rebuild, failing_block
            );
        } else {
            config.sequencer_config.block_rebuild = Some(RebuildBlocksConfig {
                from_block: failing_block,
                blocks_to_empty: vec![failing_block],
            });
        }
    }
}
