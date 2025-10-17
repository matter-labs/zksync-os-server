use smart_config::value::ExposeSecret;
use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, Environment};
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use zksync_os_observability::PrometheusExporterConfig;
use zksync_os_server::config::{
    BatcherConfig, Config, GeneralConfig, GenesisConfig, L1SenderConfig, L1WatcherConfig,
    LogConfig, MempoolConfig, ProverApiConfig, ProverInputGeneratorConfig, RpcConfig,
    SequencerConfig, StateBackendConfig, StatusServerConfig, TxValidatorConfig,
};
use zksync_os_server::run;
use zksync_os_server::sentry::init_sentry;
use zksync_os_server::zkstack_config::ZkStackConfig;
use zksync_os_state::StateHandle;
use zksync_os_state_full_diffs::FullDiffsState;

const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
pub async fn main() {
    // =========== load configs ===========
    let config = build_configs();

    // =========== init tracing ===========
    zksync_os_tracing::Tracer::new(config.log_config.format, config.log_config.use_color).init();
    tracing::info!(?config, "Loaded config");

    let prometheus: PrometheusExporterConfig =
        PrometheusExporterConfig::pull(config.general_config.prometheus_port);

    // =========== init interruption channel ===========

    // todo: implement interruption handling in other tasks
    let (stop_sender, stop_receiver) = watch::channel(false);
    // ======= Run tasks ===========
    let main_stop = stop_receiver.clone(); // keep original for Prometheus

    let _sentry_guard = config
        .general_config
        .sentry_url
        .clone()
        .map(|sentry_url| init_sentry(&sentry_url));

    let main_task = async move {
        match config.general_config.state_backend {
            StateBackendConfig::FullDiffs => run::<FullDiffsState>(main_stop.clone(), config).await,
            StateBackendConfig::Compacted => run::<StateHandle>(main_stop.clone(), config).await,
        }
    };

    let stop_receiver_copy = stop_receiver.clone();

    tokio::select! {
        _ = main_task => {
            if *stop_receiver_copy.borrow() {
                tracing::info!("Main task exited gracefully after stop signal");
                // sleep to wait for other tasks to finish
                tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
            } else {
                tracing::warn!("Main task unexpectedly exited")
            }
        },
        _ = handle_delayed_termination(stop_sender) => {},
        res = prometheus.run(stop_receiver) => {
            match res {
                Ok(_) => {
                    if *stop_receiver_copy.borrow() {
                        tracing::info!("Prometheus exporter exited gracefully after stop signal");
                        // sleep to wait for other tasks to finish
                        tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
                    } else {
                        tracing::warn!("Prometheus exporter unexpectedly exited")
                    }
                },
                Err(err) => tracing::error!(?err, "Prometheus exporter failed"),
            }
        },
    }
}

async fn handle_delayed_termination(stop_sender: watch::Sender<bool>) {
    // sigint is sent on Ctrl+C
    let mut sigint =
        signal(SignalKind::interrupt()).expect("failed to register interrupt signal handler");

    // sigterm is sent on `kill <pid>` or by kubernetes during pod shutdown
    let mut sigterm =
        signal(SignalKind::terminate()).expect("failed to register terminate signal handler");
    tokio::select! {
        _ = sigint.recv() => {
            tracing::info!("Received SIGINT, shutting down immediately");
        },
        _ = sigterm.recv() => {
            tracing::info!("Received SIGTERM: scheduling shutdown in 10s");

            stop_sender
                .send(true)
                .expect("failed to send terminate signal");

            tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
        },
    }
}

fn build_configs() -> Config {
    // todo: change with the idiomatic approach
    let mut schema = ConfigSchema::default();
    schema
        .insert(&GeneralConfig::DESCRIPTION, "general")
        .expect("Failed to insert general config");
    schema
        .insert(&GenesisConfig::DESCRIPTION, "genesis")
        .expect("Failed to insert genesis config");
    schema
        .insert(&RpcConfig::DESCRIPTION, "rpc")
        .expect("Failed to insert rpc config");
    schema
        .insert(&MempoolConfig::DESCRIPTION, "mempool")
        .expect("Failed to insert mempool config");
    schema
        .insert(&TxValidatorConfig::DESCRIPTION, "tx_validator")
        .expect("Failed to insert tx_validator config");
    schema
        .insert(&SequencerConfig::DESCRIPTION, "sequencer")
        .expect("Failed to insert sequencer config");
    schema
        .insert(&L1SenderConfig::DESCRIPTION, "l1_sender")
        .expect("Failed to insert l1_sender config");
    schema
        .insert(&L1WatcherConfig::DESCRIPTION, "l1_watcher")
        .expect("Failed to insert l1_watcher config");
    schema
        .insert(&BatcherConfig::DESCRIPTION, "batcher")
        .expect("Failed to insert batcher config");
    schema
        .insert(
            &ProverInputGeneratorConfig::DESCRIPTION,
            "prover_input_generator",
        )
        .expect("Failed to insert prover_input_generator config");
    schema
        .insert(&ProverApiConfig::DESCRIPTION, "prover_api")
        .expect("Failed to insert prover api config");
    schema
        .insert(&StatusServerConfig::DESCRIPTION, "status_server")
        .expect("Failed to insert status server config");
    schema
        .insert(&LogConfig::DESCRIPTION, "log")
        .expect("Failed to insert log config");

    let repo = ConfigRepository::new(&schema).with(Environment::prefixed(""));

    let mut general_config = repo
        .single::<GeneralConfig>()
        .expect("Failed to load general config")
        .parse()
        .expect("Failed to parse general config");

    let mut genesis_config = repo
        .single::<GenesisConfig>()
        .expect("Failed to load genesis config")
        .parse()
        .expect("Failed to parse genesis config");

    let mut rpc_config = repo
        .single::<RpcConfig>()
        .expect("Failed to load rpc config")
        .parse()
        .expect("Failed to parse rpc config");

    let mempool_config = repo
        .single::<MempoolConfig>()
        .expect("Failed to load mempool config")
        .parse()
        .expect("Failed to parse mempool config");

    let tx_validator_config = repo
        .single::<TxValidatorConfig>()
        .expect("Failed to load tx validator config")
        .parse()
        .expect("Failed to parse tx validator config");

    let mut sequencer_config = repo
        .single::<SequencerConfig>()
        .expect("Failed to load sequencer config")
        .parse()
        .expect("Failed to parse sequencer config");

    let mut l1_sender_config = repo
        .single::<L1SenderConfig>()
        .expect("Failed to load L1 sender config")
        .parse()
        .expect("Failed to parse L1 sender config");

    let l1_watcher_config = repo
        .single::<L1WatcherConfig>()
        .expect("Failed to load L1 watcher config")
        .parse()
        .expect("Failed to parse L1 watcher config");

    let batcher_config = repo
        .single::<BatcherConfig>()
        .expect("Failed to load L1 watcher config")
        .parse()
        .expect("Failed to parse L1 watcher config");

    let prover_input_generator_config = repo
        .single::<ProverInputGeneratorConfig>()
        .expect("Failed to load ProverInputGenerator config")
        .parse()
        .expect("Failed to parse ProverInputGenerator config");

    let mut prover_api_config = repo
        .single::<ProverApiConfig>()
        .expect("Failed to load prover api config")
        .parse()
        .expect("Failed to parse prover api config");

    let status_server_config = repo
        .single::<StatusServerConfig>()
        .expect("Failed to load status server config")
        .parse()
        .expect("Failed to parse status server config");

    let log_config = repo
        .single::<LogConfig>()
        .expect("Failed to load log config")
        .parse()
        .expect("Failed to parse log config");

    if let Some(config_dir) = general_config.zkstack_cli_config_dir.clone() {
        // If set, then update the configs based off the values from the yaml files.
        // This is a temporary measure until we update zkstack cli (or create a new tool) to create
        // configs that are specific to the new sequencer.
        let config = ZkStackConfig::new(config_dir.clone());
        config
            .update(
                &mut general_config,
                &mut sequencer_config,
                &mut rpc_config,
                &mut l1_sender_config,
                &mut genesis_config,
                &mut prover_api_config,
            )
            .unwrap_or_else(|_| panic!("Failed to load zkstack config from `{config_dir}`: "));
    }

    // Validate that operator keys are different
    if l1_sender_config.operator_commit_pk.expose_secret()
        == l1_sender_config.operator_prove_pk.expose_secret()
        || l1_sender_config.operator_prove_pk.expose_secret()
            == l1_sender_config.operator_execute_pk.expose_secret()
        || l1_sender_config.operator_execute_pk.expose_secret()
            == l1_sender_config.operator_commit_pk.expose_secret()
    {
        // important: don't replace this with `assert_ne` etc - it may expose private keys in logs
        panic!("Operator addresses for commit, prove and execute must be different");
    }

    Config {
        general_config,
        genesis_config,
        rpc_config,
        mempool_config,
        tx_validator_config,
        sequencer_config,
        l1_sender_config,
        l1_watcher_config,
        batcher_config,
        prover_input_generator_config,
        prover_api_config,
        status_server_config,
        log_config,
    }
}
