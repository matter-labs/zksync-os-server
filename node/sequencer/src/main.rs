use futures_util::TryFutureExt;
use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, Environment};
use std::time::Duration;
use tokio::sync::watch;
use zksync_os_sequencer::api::run_jsonrpsee_server;
use zksync_os_sequencer::config::{RpcConfig, SequencerConfig, StateConfig};
use zksync_os_sequencer::mempool::{forced_deposit_transaction, Mempool};
use zksync_os_sequencer::run_sequencer_actor;
use zksync_os_sequencer::storage::block_replay_storage::{
    BlockReplayColumnFamily, BlockReplayStorage,
};
use zksync_os_sequencer::storage::persistent_storage_map::{PersistentStorageMap, StorageMapCF};
use zksync_os_sequencer::storage::rocksdb_preimages::{PreimagesCF, RocksDbPreimages};
use zksync_os_sequencer::storage::StateHandle;
use zksync_storage::RocksDB;
use zksync_vlog::prometheus::PrometheusExporterConfig;

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const STATE_STORAGE_DB_NAME: &str = "state";
const PREIMAGES_STORAGE_DB_NAME: &str = "preimages";
// const TREE_DB_NAME: &str = "tree";

#[tokio::main]
pub async fn main() {
    let mut schema = ConfigSchema::default();
    schema
        .insert(&RpcConfig::DESCRIPTION, "rpc")
        .expect("Failed to insert rpc config");
    schema
        .insert(&StateConfig::DESCRIPTION, "state")
        .expect("Failed to insert rpc config");
    schema
        .insert(&SequencerConfig::DESCRIPTION, "sequencer")
        .expect("Failed to insert rpc config");

    let repo = ConfigRepository::new(&schema).with(Environment::default());

    let rpc_config = repo
        .single::<RpcConfig>()
        .expect("Failed to load rpc config")
        .parse()
        .expect("Failed to parse rpc config");

    let state_config = repo
        .single::<StateConfig>()
        .expect("Failed to load state config")
        .parse()
        .expect("Failed to parse state config");

    let sequencer_config = repo
        .single::<SequencerConfig>()
        .expect("Failed to load sequencer config")
        .parse()
        .expect("Failed to parse sequencer config");

    let prometheus: PrometheusExporterConfig = PrometheusExporterConfig::pull(3312);

    let (_stop_sender, stop_receiver) = watch::channel(false);
    tokio::task::spawn(
        prometheus
            .run(stop_receiver)
            .map_ok(|_| tracing::error!("unexp"))
            .map_err(|e| {
                tracing::error!("Prometheus exporter failed: {e:#}");
            }),
    );

    tracing_subscriber::fmt().init();

    let block_replay_storage_rocks_db = RocksDB::<BlockReplayColumnFamily>::new(
        &sequencer_config
            .rocks_db_path
            .join(BLOCK_REPLAY_WAL_DB_NAME),
    )
    .expect("Failed to open BlockReplayWAL")
    .with_sync_writes();

    let block_replay_storage = BlockReplayStorage::new(block_replay_storage_rocks_db);

    let state_db =
        RocksDB::<StorageMapCF>::new(&sequencer_config.rocks_db_path.join(STATE_STORAGE_DB_NAME))
            .expect("Failed to open State DB");
    let persistent_storage_map = PersistentStorageMap::new(state_db);

    let preimages_db = RocksDB::<PreimagesCF>::new(
        &sequencer_config
            .rocks_db_path
            .join(PREIMAGES_STORAGE_DB_NAME),
    )
    .expect("Failed to open Preimages DB");
    let rocks_db_preimages = RocksDbPreimages::new(preimages_db);

    let state_db_block = persistent_storage_map.rocksdb_block_number();
    let preimages_db_block = rocks_db_preimages.rocksdb_block_number();
    assert!(
        state_db_block <= preimages_db_block,
        "State DB block number ({state_db_block}) is greater than Preimages DB block number ({preimages_db_block}). This is not allowed."
    );

    let state_handle = StateHandle::new(
        state_db_block,
        persistent_storage_map,
        rocks_db_preimages,
        state_config.blocks_to_retain_in_memory,
    );

    let block_to_start = state_db_block + 1;
    tracing::info!(
        "State DB block number: {state_db_block}, Preimages DB block number: {preimages_db_block}, starting execution from {block_to_start}"
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
    // );

    tokio::select! {
        // todo: only start after the sequencer caught up?
        // ── JSON-RPC task ────────────────────────────────────────────────
        res = run_jsonrpsee_server(state_handle.clone(), mempool.clone(), block_replay_storage.clone(), rpc_config) => {
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
            block_to_start,
            block_replay_storage,
            state_handle.clone(),
            mempool,
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

    }
}
