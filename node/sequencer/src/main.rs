use futures::future::BoxFuture;
use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, Environment};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use zk_os_forward_system::run::BatchOutput;
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_l1_sender::{L1Sender, L1SenderHandle};
use zksync_os_l1_watcher::{L1Watcher, L1WatcherConfig};
use zksync_os_sequencer::api::run_jsonrpsee_server;
use zksync_os_sequencer::block_replay_storage::{BlockReplayColumnFamily, BlockReplayStorage};
use zksync_os_sequencer::config::{BatcherConfig, ProverApiConfig, RpcConfig, SequencerConfig};
use zksync_os_sequencer::execution::transaction_stream_provider::TransactionStreamProvider;
use zksync_os_sequencer::finality::FinalityTracker;
use zksync_os_sequencer::model::{BatchJob, ReplayRecord};
use zksync_os_sequencer::repositories::RepositoryManager;
use zksync_os_sequencer::run_sequencer_actor;
use zksync_os_sequencer::tree_manager::TreeManager;
use zksync_os_state::{StateConfig, StateHandle};
use zksync_os_types::forced_deposit_transaction;
use zksync_storage::RocksDB;
use zksync_vlog::prometheus::PrometheusExporterConfig;

use zksync_os_sequencer::batcher::Batcher;
use zksync_os_sequencer::prover_api::proof_storage::{ProofColumnFamily, ProofStorage};
use zksync_os_sequencer::prover_api::prover_job_manager::ProverJobManager;
use zksync_os_sequencer::prover_api::prover_server;

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const TREE_DB_NAME: &str = "tree";
const PROOF_STORAGE_DB_NAME: &str = "proofs";

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

    let proof_storage_db = RocksDB::<ProofColumnFamily>::new(
        &sequencer_config.rocks_db_path.join(PROOF_STORAGE_DB_NAME),
    )
    .expect("Failed to open ProofStorageDB");

    let proof_storage = ProofStorage::new(proof_storage_db);
    tracing::info!(
        "Proof storage initialized with {} proofs already present",
        proof_storage.get_blocks_with_proof().len()
    );

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

    // ======= Initialize async channels  ===========

    // todo: this is received by batcher and then fanned out to workers. Could just use broadcast instead
    let (blocks_for_batcher_sender, blocks_for_batcher_receiver) =
        tokio::sync::mpsc::channel::<(BatchOutput, ReplayRecord)>(100);

    let (batch_sender, batch_receiver) = tokio::sync::mpsc::channel::<BatchJob>(100);

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

    let (l1_mempool, mempool) = zksync_os_mempool::in_memory(forced_deposit_transaction());

    // ========== Initialize TransactionStreamProvider ===========

    let next_l1_priority_id = block_replay_storage.last_l1_priority_id().map(|n| n + 1).unwrap_or_default();
    let tx_stream_provider = TransactionStreamProvider::new(
        next_l1_priority_id,
        l1_mempool.clone(),
        mempool.clone(),
    );

    // ========== Initialize L1 watcher (fallible) ===========

    let l1_watcher = L1Watcher::new(l1_watcher_config, l1_mempool.clone()).await;
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

    // ========== Initialize L1 sender (fallible) ===========

    let l1_sender = L1Sender::new(l1_sender_config).await;
    let (l1_sender_task, l1_sender_handle, _last_committed_batch_number): (
        BoxFuture<anyhow::Result<()>>,
        Option<L1SenderHandle>,
        u64,
    ) = match l1_sender {
        Ok((l1_sender, l1_handle, last_committed_batch_number)) => (
            Box::pin(l1_sender.run()),
            Some(l1_handle),
            last_committed_batch_number,
        ),
        Err(err) => {
            tracing::error!(?err, "failed to start L1 sender; proceeding without it");
            let mut stop_receiver = stop_receiver.clone();
            (
                Box::pin(async move {
                    // Defer until we receive stop signal, i.e. a task that does nothing
                    stop_receiver
                        .changed()
                        .await
                        .map_err(|e| anyhow::anyhow!(e))
                }),
                None,
                0,
            )
        }
    };

    // ========== Initialize batcher (aka prover_input_generator) (if configured) ===========

    let batcher_task: BoxFuture<anyhow::Result<()>> = if batcher_config.component_enabled {
        // TODO: Start from `last_committed_batch_number`
        let batcher = Batcher::new(
            blocks_for_batcher_receiver,
            batch_sender,
            l1_sender_handle,
            state_handle.clone(),
            // MerkleTreeReader::new(tree_wrapper.clone()).expect("cannot init MerkleTreeReader"),
            batcher_config.logging_enabled,
            batcher_config.num_workers,
        );
        Box::pin(batcher.run_loop())
    } else {
        tracing::info!(
            "Batcher disabled via configuration; draining channel to avoid backpressure"
        );
        let mut receiver = blocks_for_batcher_receiver;
        Box::pin(async move {
            while receiver.recv().await.is_some() {
                // Drop messages silently to prevent backpressure
            }
            Ok(())
        })
    };

    // ======= Initialize Prover Api Server (todo: should be optional) ========

    let prover_job_manager = Arc::new(ProverJobManager::new(
        proof_storage,
        prover_api_config.job_timeout,
        prover_api_config.max_unproved_blocks,
    ));
    let prover_server_job =
        prover_server::run(prover_job_manager.clone(), prover_api_config.address);

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

        // ── Prover Server tasks (todo: should be conditioned) ──────────────────────────────────
        _res = prover_job_manager.listen_for_batch_jobs(batch_receiver) => {
            tracing::warn!("Prover job manager task exited")
        }

        res = prover_server_job => {
            match res {
                Ok(_)  => tracing::warn!("prover_server_job task exited"),
                Err(e) => tracing::error!("prover_server_job task failed: {e:#}"),
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

        // ── L1 sender task ────────────────────────────────────────────────
        res = l1_sender_task => {
            match res {
                Ok(_)  => tracing::warn!("L1 sender unexpectedly exited"),
                Err(e) => tracing::error!("L1 sender failed: {e:#}"),
            }
        }

        // ── Sequencer task ───────────────────────────────────────────────
        res = run_sequencer_actor(
            first_block_to_execute,
            blocks_for_batcher_sender,
            tree_sender,
            tx_stream_provider,
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
