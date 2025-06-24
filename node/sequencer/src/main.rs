use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, Environment};
use std::cmp::min;
use std::path::Path;
use std::time::Duration;
use tokio::sync::watch;
use zk_os_forward_system::run::output::BatchResult;
use zk_os_forward_system::run::{BatchOutput, StorageWrite};
use zksync_os_merkle_tree::{MerkleTreeColumnFamily, RocksDBWrapper};
use zksync_os_sequencer::api::run_jsonrpsee_server;
use zksync_os_sequencer::block_replay_storage::{BlockReplayColumnFamily, BlockReplayStorage};
use zksync_os_sequencer::config::{RpcConfig, SequencerConfig};
use zksync_os_sequencer::finality::FinalityTracker;
use zksync_os_sequencer::mempool::{forced_deposit_transaction, Mempool};
use zksync_os_sequencer::model::ReplayRecord;
use zksync_os_sequencer::repositories::RepositoryManager;
use zksync_os_sequencer::run_sequencer_actor;
use zksync_os_sequencer::tree_manager::TreeManager;
use zksync_os_state::{StateConfig, StateHandle};
use zksync_storage::{RocksDB, RocksDBOptions, StalledWritesRetries};
use zksync_vlog::prometheus::PrometheusExporterConfig;

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";

const TREE_DB_NAME: &str = "tree";

#[tokio::main]
pub async fn main() {
    tracing_subscriber::fmt().init();

    // =========== load configs ===========
    // todo: change with the idiomatic approach
    let mut schema = ConfigSchema::default();
    schema
        .insert(&RpcConfig::DESCRIPTION, "rpc")
        .expect("Failed to insert rpc config");
    schema
        .insert(&SequencerConfig::DESCRIPTION, "sequencer")
        .expect("Failed to insert rpc config");

    let repo = ConfigRepository::new(&schema).with(Environment::prefixed(""));

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

    let prometheus: PrometheusExporterConfig = PrometheusExporterConfig::pull(3312);

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

    // ======= Initialize batcher- and tree- related channels ===========

    let (batch_output_sender, batch_output_receiver) =
        tokio::sync::mpsc::channel::<BatchOutput>(10);
    let (block_replay_sender, block_replay_receiver) =
        tokio::sync::mpsc::channel::<ReplayRecord>(100000);

    // ========== Initialize tree manager ===========

    let tree_manager = TreeManager::new(
        Path::new(&sequencer_config.rocks_db_path.join(TREE_DB_NAME)),
        batch_output_receiver,
    );

    let tree_last_processed_block = tree_manager
        .last_processed_block()
        .expect("cannot read tree last processed block after initialization");

    let first_block_to_execute = [
        storage_map_block,
        preimages_block,
        tree_last_processed_block,
    ]
    .iter()
    .min()
    .unwrap()
        + 1;

    // ========== Initialize block finality trackers ===========

    let finality_tracker = FinalityTracker::new(wal_block.unwrap_or(0));

    // note: unfinished feature, not really used yet
    tracing::info!(
        storage_map_block = storage_map_block,
        preimages_block = preimages_block,
        wal_block = wal_block,
        canonized_block = finality_tracker.get_canonized_block(),
        tree_last_processed_block = tree_last_processed_block,
        first_block_to_execute = first_block_to_execute,
        "▶ Storage read. Node starting."
    );

    let mempool = Mempool::new(forced_deposit_transaction());

    // ======= Run tasks ===========

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
        res = tree_manager.run_loop() => {
            match res {
                Ok(_)  => tracing::warn!("TREE server unexpectedly exited"),
                Err(e) => tracing::error!("TREE server failed: {e:#}"),
            }
        }

        // ── Sequencer task ───────────────────────────────────────────────
        res = run_sequencer_actor(
            first_block_to_execute,
            batch_output_sender,
            block_replay_sender,
            mempool,
            state_handle.clone(),
            block_replay_storage,
            repositories,
            finality_tracker,
            sequencer_config
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
            .run(stop_receiver) => {
            match res {
                Ok(_)  => tracing::warn!("Prometheus exporter unexpectedly exited"),
                Err(e) => tracing::error!("Prometheus exporter failed: {e:#}"),
            }
        }
    }
}
