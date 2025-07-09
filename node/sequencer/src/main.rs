use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, Environment};
use tokio::sync::watch;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_l1_watcher::L1WatcherConfig;
use zksync_os_sequencer::config::{
    BatcherConfig, MempoolConfig, ProverApiConfig, RpcConfig, SequencerConfig,
};
use zksync_os_sequencer::run;
use zksync_vlog::prometheus::PrometheusExporterConfig;

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
        rpc_config,
        mempool_config,
        sequencer_config,
        l1_sender_config,
        l1_watcher_config,
        batcher_config,
        prover_api_config,
    ) = build_configs();

    let prometheus: PrometheusExporterConfig = PrometheusExporterConfig::pull(3312);

    // =========== init interruption channel ===========

    // todo: implement interruption handling in other tasks
    let (_stop_sender, stop_receiver) = watch::channel(false);

    // ======= Run tasks ===========
    tokio::select! {
        // ── Main task ───────────────────────────────────────────────
        _ = run(
            stop_receiver.clone(),
            rpc_config,
            mempool_config,
            sequencer_config,
            l1_sender_config,
            l1_watcher_config,
            batcher_config,
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
    RpcConfig,
    MempoolConfig,
    SequencerConfig,
    L1SenderConfig,
    L1WatcherConfig,
    BatcherConfig,
    ProverApiConfig,
) {
    // todo: change with the idiomatic approach
    let mut schema = ConfigSchema::default();
    schema
        .insert(&RpcConfig::DESCRIPTION, "rpc")
        .expect("Failed to insert rpc config");
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
        .insert(&ProverApiConfig::DESCRIPTION, "prover_api")
        .expect("Failed to insert prover api config");

    let repo = ConfigRepository::new(&schema).with(Environment::prefixed(""));

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

    let prover_api_config = repo
        .single::<ProverApiConfig>()
        .expect("Failed to load prover api config")
        .parse()
        .expect("Failed to parse prover api config");
    (
        rpc_config,
        mempool_config,
        sequencer_config,
        l1_sender_config,
        l1_watcher_config,
        batcher_config,
        prover_api_config,
    )
}
