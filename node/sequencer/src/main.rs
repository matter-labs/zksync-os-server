use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, Environment};
use tokio::sync::watch;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_l1_watcher::L1WatcherConfig;
use zksync_os_observability::PrometheusExporterConfig;
use zksync_os_sequencer::config::{
    BatcherConfig, GenesisConfig, MempoolConfig, ProverApiConfig, ProverInputGeneratorConfig,
    RpcConfig, SequencerConfig,
};
use zksync_os_sequencer::run;

#[tokio::main]
pub async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    // =========== load configs ===========
    let (
        genesis_config,
        rpc_config,
        mempool_config,
        sequencer_config,
        l1_sender_config,
        l1_watcher_config,
        batcher_config,
        prover_input_generator_config,
        prover_api_config,
    ) = build_configs();

    let prometheus: PrometheusExporterConfig =
        PrometheusExporterConfig::pull(sequencer_config.prometheus_port);

    // =========== init interruption channel ===========

    // todo: implement interruption handling in other tasks
    let (_stop_sender, stop_receiver) = watch::channel(false);

    // ======= Run tasks ===========
    tokio::select! {
        // ── Main task ───────────────────────────────────────────────
        _ = run(
            stop_receiver.clone(),
            genesis_config,
            rpc_config,
            mempool_config,
            sequencer_config,
            l1_sender_config,
            l1_watcher_config,
            batcher_config,
            prover_input_generator_config,
            prover_api_config
        ) => {}

        // ── Prometheus task ─────────────────────────────────────────
        res = prometheus
            .run(stop_receiver) => {
            match res {
                Ok(_)  => tracing::warn!("Prometheus exporter unexpectedly exited"),
                Err(e) => tracing::error!("Prometheus exporter failed: {e:#}"),
            }
        }
    }
}

fn build_configs() -> (
    GenesisConfig,
    RpcConfig,
    MempoolConfig,
    SequencerConfig,
    L1SenderConfig,
    L1WatcherConfig,
    BatcherConfig,
    ProverInputGeneratorConfig,
    ProverApiConfig,
) {
    // todo: change with the idiomatic approach
    let mut schema = ConfigSchema::default();
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

    let repo = ConfigRepository::new(&schema).with(Environment::prefixed(""));

    let genesis_config = repo
        .single::<GenesisConfig>()
        .expect("Failed to load genesis config")
        .parse()
        .expect("Failed to parse genesis config");

    let rpc_config = repo
        .single::<RpcConfig>()
        .expect("Failed to load rpc config")
        .parse()
        .expect("Failed to parse rpc config");

    let mempool_config = repo
        .single::<MempoolConfig>()
        .expect("Failed to load mempool config")
        .parse()
        .expect("Failed to parse mempool config");

    let sequencer_config = repo
        .single::<SequencerConfig>()
        .expect("Failed to load sequencer config")
        .parse()
        .expect("Failed to parse sequencer config");

    let l1_sender_config = repo
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

    let prover_api_config = repo
        .single::<ProverApiConfig>()
        .expect("Failed to load prover api config")
        .parse()
        .expect("Failed to parse prover api config");

    (
        genesis_config,
        rpc_config,
        mempool_config,
        sequencer_config,
        l1_sender_config,
        l1_watcher_config,
        batcher_config,
        prover_input_generator_config,
        prover_api_config,
    )
}
