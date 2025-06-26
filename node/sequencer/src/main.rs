use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, Environment, Yaml};
use std::cmp::min;
use std::time::Duration;
use anyhow::Context;
use futures::channel::mpsc;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;
use zksync_concurrency::{ctx, scope, sync};
use zksync_os_sequencer::api::run_jsonrpsee_server;
use zksync_os_sequencer::block_replay_storage::{BlockReplayColumnFamily, BlockReplayStorage};
use zksync_os_sequencer::config::{RpcConfig, SequencerConfig};
use zksync_os_sequencer::mempool::{forced_deposit_transaction, Mempool};
use zksync_os_sequencer::repositories::RepositoryManager;
use zksync_os_sequencer::run_sequencer_actor;
use zksync_os_sequencer::consensus::{run_main_node, ConsensusConfig, ConsensusSecrets, ConsensusStorage, ReducedBlockCommand};
use zksync_os_state::{StateConfig, StateHandle};
use zksync_storage::RocksDB;
use zksync_vlog::prometheus::PrometheusExporterConfig;
use zksync_os_sequencer::finality::FinalityTracker;

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const CONSENSUS_DB_NAME: &str = "consensus";
const FILE_CONFIG_PATH: &str = "./default_config.yaml";

fn file_config_path() -> String {
    if let Ok(x) = std::env::var("ZKSYNC_CONFIG_PATH") {
        x
    } else {
        FILE_CONFIG_PATH.to_string()
    }
}

#[tokio::main]
pub async fn main() {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();

    // =========== load configs ===========
    // todo: change with the idiomatic approach
    let mut schema = ConfigSchema::default();
    schema
        .insert(&RpcConfig::DESCRIPTION, "rpc")
        .expect("Failed to insert rpc config");
    schema
        .insert(&SequencerConfig::DESCRIPTION, "sequencer")
        .expect("Failed to insert rpc config");
    schema
        .insert(&ConsensusConfig::DESCRIPTION, "consensus")
        .expect("Failed to insert rpc config");
    schema
        .insert(&ConsensusSecrets::DESCRIPTION, "consensus_secrets")
        .expect("Failed to insert rpc config");

    let file_config_path = file_config_path();
    let yaml_path = std::path::Path::new(&file_config_path);
    let yaml = read_yaml(yaml_path).unwrap();

    let repo = ConfigRepository::new(&schema)
        .with(Environment::prefixed(""))
        .with(yaml);

    let rpc_config = repo
        .single::<RpcConfig>()
        .expect("Failed to load rpc config")
        .parse()
        .expect("Failed to parse rpc config");

    let sequencer_config = repo
        .single::<SequencerConfig>()
        .expect("Failed to load sequencer config")
        .parse()
        .expect("Failed to parse sequencer config");

    let consensus_config = repo
        .single::<ConsensusConfig>()
        .map(|c| c.parse().expect("Failed to parse consensus config"))
        .ok();

    let consensus_secrets = repo
        .single::<ConsensusSecrets>()
        .expect("Failed to load consensus secrets")
        .parse()
        .expect("Failed to parse consensus secrets");

    let prometheus: PrometheusExporterConfig = PrometheusExporterConfig::pull(sequencer_config.prometheus_exporter_port);

    // =========== init interruption channel ===========

    // todo: implement interruption handling in other tasks
    let (_stop_sender, stop_receiver) = watch::channel(false);

    // =========== load DBs ===========

    let block_replay_storage_rocks_db = RocksDB::<BlockReplayColumnFamily>::new(
        &sequencer_config
            .rocks_db_path
            .join(BLOCK_REPLAY_WAL_DB_NAME),
    )
    .expect("Failed to open BlockReplayWAL")
    .with_sync_writes();
    let block_replay_storage = BlockReplayStorage::new(block_replay_storage_rocks_db);

    let consensus_storage = ConsensusStorage::new(
        &sequencer_config
            .rocks_db_path
            .join(CONSENSUS_DB_NAME),
        block_replay_storage.clone(),
    );

    let state_handle = StateHandle::new(StateConfig {
        blocks_to_retain_in_memory: sequencer_config.blocks_to_retain_in_memory,
        rocks_db_path: sequencer_config.rocks_db_path.clone(),
    });

    let repositories = RepositoryManager::new(sequencer_config.blocks_to_retain_in_memory);

    // =========== load last persisted block numbers.  ===========
    let (storage_map_block, preimages_block) = state_handle.latest_block_numbers();
    let wal_block = block_replay_storage.latest_block();

    // todo: will be used once repositories have persistence
    // let repository_blocks = ...

    // it's enough to check a weaker condition (`>= min`) - but currently neither state components can be ahead of WAL
    assert!(
        wal_block.unwrap_or(0) >= storage_map_block,
        "State DB block number ({storage_map_block}) is greater than WAL block ({wal_block:?}). Preimages block: ({preimages_block})"
    );

    let first_block_to_execute = min(storage_map_block, preimages_block) + 1;

    // ========== Initialize block finality trackers ===========
    
    let finality_tracker = FinalityTracker::new(wal_block.unwrap_or(0));

    tracing::info!(
        storage_map_block = storage_map_block,
        preimages_block = preimages_block,
        wal_block = wal_block,
        canonized_block = finality_tracker.get_canonized_block(),
        first_block_to_execute = first_block_to_execute,
        "▶ Storage read. Node starting."
    );

    let mempool = Mempool::new(forced_deposit_transaction());

    // Sequencer will not run the tree - batcher will (other component, another machine)
    // running it for now just to test the performance
    // let db: RocksDB<MerkleTreeColumnFamily> = RocksDB::with_options(
    //     Path::new(&sequencer_config.rocks_db_path.join(TREE_DB_NAME)),
    //     RocksDBOptions {
    //         block_cache_capacity: Some(128 << 20),
    //         include_indices_and_filters_in_block_cache: false,
    //         large_memtable_capacity: Some(256 << 20),
    //         stalled_writes_retries: StalledWritesRetries::new(Duration::from_secs(10)),
    //         max_open_files: None,
    //     },
    // ).unwrap();
    // let tree_wrapper = RocksDBWrapper::from(db);
    // let mut tree_manager = TreeManager::new(
    //     tree_wrapper,
    //     state_handle.clone(),
    //     // this is a lie - we don't know the actual last block that was processed before restaty,
    //     // but we only use a tree for performance measure so it's ok
    //     state_handle.last_canonized_block_number(),

    let (consensus_command_sender, consensus_command_receiver) =
        mpsc::unbounded::<ReducedBlockCommand>();
    let consensus_command_receiver = if consensus_config.is_some() {
        Some(consensus_command_receiver)
    } else {
        None
    };

    tokio::select! {
        // todo: only start after the sequencer caught up?
        // ── JSON-RPC task ────────────────────────────────────────────────
        res = run_jsonrpsee_server(
            rpc_config,
            repositories.clone(),
            finality_tracker.clone(),
            state_handle.clone(),
            mempool.clone(),
            block_replay_storage.clone()) => {
            match res {
                Ok(_)  => tracing::warn!("JSON-RPC server unexpectedly exited"),
                Err(e) => tracing::error!("JSON-RPC server failed: {e:#}"),
            }
        }

        // ── TREE task ────────────────────────────────────────────────
        // res = tree_manager.run_loop() => {
        //     match res {
        //         Ok(_)  => tracing::warn!("TREE server unexpectedly exited"),
        //         Err(e) => tracing::error!("TREE server failed: {e:#}"),
        //     }
        // }

        // ── Sequencer task ───────────────────────────────────────────────
        res = run_sequencer_actor(
            first_block_to_execute,
            mempool,
            state_handle.clone(),
            block_replay_storage,
            repositories,
            finality_tracker,
            sequencer_config,
            consensus_command_receiver,
        ) => {
            match res {
                Ok(_)  => tracing::warn!("Sequencer server unexpectedly exited"),
                Err(e) => tracing::error!("Sequencer server failed: {e:#}"),
            }
        }

        _ = state_handle.collect_state_metrics(Duration::from_secs(2)) => {
            tracing::warn!("collect_state_metrics unexpectedly exited")
        }
        _ = state_handle.compact_periodically(Duration::from_millis(100)) => {
            tracing::warn!("compact_periodically unexpectedly exited")
        }

        res = prometheus
            .run(stop_receiver.clone()) => {
            match res {
                Ok(_)  => tracing::warn!("Prometheus exporter unexpectedly exited"),
                Err(e) => tracing::error!("Prometheus exporter failed: {e:#}"),
            }
        }

        res = run_consensus(consensus_config, consensus_secrets, consensus_storage, consensus_command_sender, stop_receiver), if consensus_config.is_some() => {
            match res {
                Ok(_)  => tracing::warn!("Consensus unexpectedly exited"),
                Err(e) => tracing::error!("Consensus failed: {e:#}"),
            }
        }
    }
}

fn read_yaml(path: &std::path::Path) -> anyhow::Result<Yaml> {
    let file =
        std::fs::File::open(path).with_context(|| format!("failed opening config file {path:?}"))?;
    let raw: serde_yaml::Mapping = serde_yaml::from_reader(std::io::BufReader::new(file))
        .with_context(|| format!("failed reading YAML map from {path:?}"))?;
    let filename = path.as_os_str().to_string_lossy();
    Yaml::new(&filename, raw)
        .with_context(|| format!("failed digesting YAML map from {path:?}"))
}

async fn run_consensus(
    config: Option<ConsensusConfig>,
    secrets: ConsensusSecrets,
    storage: ConsensusStorage,
    command_sender: mpsc::UnboundedSender<ReducedBlockCommand>,
    mut stop_receiver: watch::Receiver<bool>,
) -> anyhow::Result<bool> {
    let Some(config) = config else {
        tracing::info!("Consensus is not configured, skipping.");
        return Ok(false);
    };
    scope::run!(&ctx::root(), |ctx, s| async move {
        s.spawn_bg(run_main_node(
            ctx,
            config,
            secrets,
            storage,
            command_sender,
        ));
        let _ = sync::wait_for(ctx, &mut stop_receiver, |stop| *stop).await;
        Ok(true)
    })
    .await
}