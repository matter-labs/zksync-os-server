use futures::future::BoxFuture;
use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, Environment};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::watch;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use zk_os_forward_system::run::BatchOutput;
use zksync_os_l1_watcher::{L1Watcher, L1WatcherConfig};
use zksync_os_merkle_tree::MerkleTreeReader;
use zksync_os_sequencer::api::run_jsonrpsee_server;
use zksync_os_sequencer::block_replay_storage::{BlockReplayColumnFamily, BlockReplayStorage};
use zksync_os_sequencer::config::{BatcherConfig, RpcConfig, SequencerConfig};
use zksync_os_sequencer::finality::FinalityTracker;
use zksync_os_sequencer::model::ReplayRecord;
use zksync_os_sequencer::repositories::RepositoryManager;
use zksync_os_sequencer::run_sequencer_actor;
use zksync_os_sequencer::tree_manager::TreeManager;
use zksync_os_state::{StateConfig, StateHandle};
use zksync_storage::RocksDB;
use zksync_types::l1::L1Tx;
use zksync_types::{Address, Execute, L1TxCommonData, PriorityOpId, Transaction, U256};
use zksync_vlog::prometheus::PrometheusExporterConfig;
use zksync_os_sequencer::batcher::Batcher;

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";

const TREE_DB_NAME: &str = "tree";

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
    // todo: change with the idiomatic approach
    let mut schema = ConfigSchema::default();
    schema
        .insert(&RpcConfig::DESCRIPTION, "rpc")
        .expect("Failed to insert rpc config");
    schema
        .insert(&SequencerConfig::DESCRIPTION, "sequencer")
        .expect("Failed to insert sequencer config");
    schema
        .insert(&L1WatcherConfig::DESCRIPTION, "l1_watcher")
        .expect("Failed to insert l1_watcher config");
    schema
        .insert(&BatcherConfig::DESCRIPTION, "batcher")
        .expect("Failed to insert batcher config");

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
        // when running batcher, we need to start from zero due to in-memory tree
        erase_storage_on_start: batcher_config.component_enabled,
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

    // ======= Initialize channel between sequencer and tree/batcher  ===========

    let (batcher_sender, batcher_receiver) =
        tokio::sync::mpsc::channel::<(BatchOutput, ReplayRecord)>(100);

    let (tree_sender, tree_receiver) = tokio::sync::mpsc::channel::<BatchOutput>(100);

    let (tree_ready_block_sender, _tree_ready_block_receiver) = watch::channel(0u64);

    // ========== Initialize tree manager ===========

    let tree_wrapper = TreeManager::tree_wrapper(Path::new(
        &sequencer_config.rocks_db_path.join(TREE_DB_NAME),
    ));
    let tree_manager =
        TreeManager::new(tree_wrapper.clone(), tree_receiver, tree_ready_block_sender);

    let tree_last_processed_block = tree_manager
        .last_processed_block()
        .expect("cannot read tree last processed block after initialization");

    let first_block_to_execute = if batcher_config.component_enabled {
        1
    } else {
        [
            storage_map_block,
            preimages_block,
            tree_last_processed_block,
        ]
        .iter()
        .min()
        .unwrap()
            + 1
    };

    // ========== Initialize block finality trackers ===========

    // note: unfinished feature, not really used yet
    let finality_tracker = FinalityTracker::new(wal_block.unwrap_or(0));

    tracing::info!(
        storage_map_block = storage_map_block,
        preimages_block = preimages_block,
        wal_block = wal_block,
        canonized_block = finality_tracker.get_canonized_block(),
        tree_last_processed_block = tree_last_processed_block,
        first_block_to_execute = first_block_to_execute,
        "▶ Storage read. Node starting."
    );

    let mempool = zksync_os_mempool::in_memory(forced_deposit_transaction());

    let l1_watcher = L1Watcher::new(l1_watcher_config, mempool.clone()).await;
    let _l1_watcher_task: BoxFuture<anyhow::Result<()>> = match l1_watcher {
        Ok(l1_watcher) => Box::pin(l1_watcher.run()),
        Err(err) => {
            tracing::error!(?err, "failed to start L1 watcher; proceeding without it");
            let mut stop_receiver = stop_receiver.clone();
            Box::pin(async move {
                // Defer until we receive stop signal, i.e. a task that does nothing
                stop_receiver
                    .changed()
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
            })
        }
    };

    // ========== Initialize batcher (conditional) ===========

    let batcher_task: BoxFuture<anyhow::Result<()>> = if batcher_config.component_enabled {
        let batcher = Batcher::new(
            batcher_receiver,
            state_handle.clone(),
            MerkleTreeReader::new(tree_wrapper.clone()).expect("cannot init MerkleTreeReader"),
            batcher_config.logging_enabled,
            batcher_config.num_workers
        );
        Box::pin(batcher.run_loop())
    } else {
        tracing::info!(
            "Batcher disabled via configuration; draining channel to avoid backpressure"
        );
        let mut receiver = batcher_receiver;
        Box::pin(async move {
            while receiver.recv().await.is_some() {
                // Drop messages silently to prevent backpressure
            }
            Ok(())
        })
    };

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

        // ── BATCHER task (may be disabled) ──────────────────────────────────
        res = batcher_task => {
            match res {
                Ok(_)  => tracing::warn!("Batcher task exited"),
                Err(e) => tracing::error!("Batcher task failed: {e:#}"),
            }
        }

        // todo: commented out for now because it affects performance - even when doing nothing
        // ── L1 Watcher task ────────────────────────────────────────────────
        // res = l1_watcher_task => {
        //     match res {
        //         Ok(_)  => tracing::warn!("L1 watcher unexpectedly exited"),
        //         Err(e) => tracing::error!("L1 watcher failed: {e:#}"),
        //     }
        // }

        // ── Sequencer task ───────────────────────────────────────────────
        res = run_sequencer_actor(
            first_block_to_execute,
            batcher_sender,
            tree_sender,
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

// to be replaced with proper L1 deposit
pub fn forced_deposit_transaction() -> Transaction {
    L1Tx {
        execute: Execute {
            contract_address: Some(
                Address::from_str("0x36615Cf349d7F6344891B1e7CA7C72883F5dc049").unwrap(),
            ),
            calldata: vec![],
            value: U256::from("100"),
            factory_deps: vec![],
        },
        common_data: L1TxCommonData {
            sender: Address::from_str("0x36615Cf349d7F6344891B1e7CA7C72883F5dc049").unwrap(),
            serial_id: PriorityOpId(1),
            layer_2_tip_fee: Default::default(),
            full_fee: U256::from("10000000000"),
            max_fee_per_gas: U256::from(1),
            gas_limit: U256::from("10000000000"),
            gas_per_pubdata_limit: U256::from(1000),
            op_processing_type: Default::default(),
            priority_queue_type: Default::default(),
            canonical_tx_hash: Default::default(),
            to_mint: U256::from("100000000000000000000000000000"),
            refund_recipient: Address::from_str("0x36615Cf349d7F6344891B1e7CA7C72883F5dc049")
                .unwrap(),
            eth_block: 0,
        },
        received_timestamp_ms: 0,
    }
    .into()
}
