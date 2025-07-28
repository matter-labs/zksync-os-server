#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

pub mod api;
pub mod batcher;
pub mod block_replay_storage;
pub mod config;
pub mod execution;
mod metrics;
pub mod model;
pub mod prover_api;
mod prover_input_generator;
pub mod repositories;
pub mod reth_state;
pub mod tree_manager;
mod util;

use crate::api::run_jsonrpsee_server;
use crate::batcher::Batcher;
use crate::block_replay_storage::{BlockReplayColumnFamily, BlockReplayStorage};
use crate::config::{
    BatcherConfig, MempoolConfig, ProverApiConfig, ProverInputGeneratorConfig, RpcConfig,
    SequencerConfig,
};
use crate::execution::{
    block_context_provider::BlockContextProvider, block_executor::execute_block,
};
use crate::metrics::GENERAL_METRICS;
use crate::model::batches::{BatchEnvelope, FriProof, ProverInput};
use crate::prover_api::fake_provers_pool::FakeProversPool;
use crate::prover_api::gapless_committer::GaplessCommitter;
use crate::prover_api::proof_storage::{ProofColumnFamily, ProofStorage};
use crate::prover_api::prover_job_manager::ProverJobManager;
use crate::prover_api::prover_server;
use crate::prover_input_generator::ProverInputGenerator;
use crate::repositories::RepositoryManager;
use crate::repositories::api_interface::ApiRepository;
use crate::reth_state::ZkClient;
use crate::tree_manager::TreeManager;
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use futures::stream::{BoxStream, StreamExt};
use model::blocks::{BlockCommand, ProduceCommand, ReplayRecord};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;
use tokio::time::Instant;
use zk_os_forward_system::run::{BatchOutput as BlockOutput, BatchOutput};
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_l1_sender::{L1Sender, L1SenderHandle};
use zksync_os_l1_watcher::{L1Watcher, L1WatcherConfig};
use zksync_os_state::{StateConfig, StateHandle};
use zksync_storage::RocksDB;

const CHAIN_ID: u64 = 270;

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const TREE_DB_NAME: &str = "tree";
const PROOF_STORAGE_DB_NAME: &str = "proofs";
const REPOSITORY_DB_NAME: &str = "repository";

#[allow(clippy::too_many_arguments)]
pub async fn run_sequencer_actor(
    starting_block: u64,

    batcher_sink: Sender<(BlockOutput, ReplayRecord)>,
    tree_sink: Sender<BlockOutput>,

    mut command_block_context_provider: BlockContextProvider,
    state: StateHandle,
    wal: BlockReplayStorage,
    repositories: RepositoryManager,
    sequencer_config: SequencerConfig,
) -> Result<()> {
    let mut stream = command_source(
        &wal,
        starting_block,
        sequencer_config.block_time,
        sequencer_config.max_transactions_in_block,
    );

    // fixme(#49): wait for l1-watcher to propagate all L1 transactions present by default
    //             delete this when we start streaming them
    tokio::time::sleep(Duration::from_secs(3)).await;

    while let Some(cmd) = stream.next().await {
        // todo: also report full latency between command invocations
        let block_number = cmd.block_number();

        tracing::info!(
            block_number,
            cmd = cmd.to_string(),
            "▶ starting command. Turning into PreparedCommand.."
        );
        let mut stage_started_at = Instant::now();

        let prepared_cmd = command_block_context_provider.process_command(cmd)?;

        tracing::debug!(
            block_number,
            starting_l1_priority_id = prepared_cmd.starting_l1_priority_id,
            "▶ Prepared command in {:?}. Executing..",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        let (block_output, replay_record, purged_txs) = execute_block(prepared_cmd, state.clone())
            .await
            .context("execute_block")?;

        tracing::info!(
            block_number,
            transactions = replay_record.transactions.len(),
            preimages = block_output.published_preimages.len(),
            storage_writes = block_output.storage_writes.len(),
            "▶ Executed after {:?}. Adding to state...",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        state.add_block_result(
            block_number,
            block_output.storage_writes.clone(),
            block_output
                .published_preimages
                .iter()
                .map(|(k, v, _)| (*k, v)),
        )?;

        tracing::debug!(
            block_number,
            "▶ Added to state in {:?}. Adding to repos...",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        // todo: do not call if api is not enabled.
        repositories
            .populate_in_memory_blocking(block_output.clone(), replay_record.transactions.clone())
            .await;

        tracing::debug!(
            block_number,
            "▶ Added to repos in {:?}. Reporting back to block_transaction_provider (mempools)...",
            stage_started_at.elapsed()
        );

        stage_started_at = Instant::now();

        // TODO: would updating mempool in parallel with state make sense?
        command_block_context_provider.on_canonical_state_change(&block_output, &replay_record);
        let purged_txs_hashes = purged_txs.into_iter().map(|(hash, _)| hash).collect();
        command_block_context_provider.remove_txs(purged_txs_hashes);

        tracing::debug!(
            block_number,
            "▶ Reported to block_transaction_provider in {:?}. Adding to wal...",
            stage_started_at.elapsed()
        );

        stage_started_at = Instant::now();

        wal.append_replay(replay_record.clone());

        tracing::debug!(
            block_number,
            "▶ Added to wal and canonized in {:?}. Sending to sinks...",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        batcher_sink
            .send((block_output.clone(), replay_record))
            .await?;
        tree_sink.send(block_output).await?;

        tracing::info!(
            block_number,
            "✔ sent to sinks in {:?}",
            stage_started_at.elapsed()
        );

        GENERAL_METRICS.block_number[&"execute"].set(block_number);
    }
    Ok::<(), anyhow::Error>(())
}

/// In decentralized case, consensus will provide a stream of
/// interleaved Replay and Produce commands instead.
/// Currently it's a stream of Replays followed by Produces
fn command_source(
    block_replay_wal: &BlockReplayStorage,
    block_to_start: u64,
    block_time: Duration,
    max_transactions_in_block: usize,
) -> BoxStream<BlockCommand> {
    let last_block_in_wal = block_replay_wal.latest_block().unwrap_or(0);
    tracing::info!(last_block_in_wal, "Last block in WAL: {last_block_in_wal}");
    tracing::info!(block_to_start, "block_to_start: {block_to_start}");

    // Stream of replay commands from WAL
    let replay_wal_stream: BoxStream<BlockCommand> =
        Box::pin(block_replay_wal.replay_commands_from(block_to_start));

    // Combined source: run WAL replay first, then produce blocks from mempool
    let produce_stream: BoxStream<BlockCommand> =
        futures::stream::unfold(last_block_in_wal + 1, move |block_number| async move {
            Some((
                BlockCommand::Produce(ProduceCommand {
                    block_number,
                    block_time,
                    max_transactions_in_block,
                }),
                block_number + 1,
            ))
        })
        .boxed();
    let stream = replay_wal_stream.chain(produce_stream);
    stream.boxed()
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    stop_receiver: watch::Receiver<bool>,
    rpc_config: RpcConfig,
    mempool_config: MempoolConfig,
    sequencer_config: SequencerConfig,
    l1_sender_config: L1SenderConfig,
    l1_watcher_config: L1WatcherConfig,
    batcher_config: BatcherConfig,
    prover_input_generator_config: ProverInputGeneratorConfig,
    prover_api_config: ProverApiConfig,
) {
    // ======= Boilerplate - Initialize async channels  ===========

    // Channel between `BlockExecutor` and `Batcher`
    let (blocks_for_batcher_sender, blocks_for_batcher_receiver) =
        tokio::sync::mpsc::channel::<(BatchOutput, ReplayRecord)>(10);

    // Channel between `BlockExecutor` and `TreeManager`
    let (tree_sender, tree_receiver) = tokio::sync::mpsc::channel::<BatchOutput>(10);

    // Channel between `Batcher` and `ProverInputGenerator`
    let (batch_replay_data_sender, batch_replay_data_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<Vec<ReplayRecord>>>(10);

    // Channel between `ProverInputGenerator` and `ProverAPI`
    let (batch_for_proving_sender, batch_for_prover_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<ProverInput>>(10);

    let (batch_with_proof_sender, batch_with_proof_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<FriProof>>(10);

    // =========== Boilerplate - initialize components that don't need state recovery  ===========
    tracing::info!("Initializing BlockReplayStorage");
    let block_replay_storage_rocks_db = RocksDB::<BlockReplayColumnFamily>::new(
        &sequencer_config
            .rocks_db_path
            .join(BLOCK_REPLAY_WAL_DB_NAME),
    )
    .expect("Failed to open BlockReplayWAL")
    .with_sync_writes();
    let block_replay_storage = BlockReplayStorage::new(block_replay_storage_rocks_db, CHAIN_ID);

    tracing::info!("Initializing StateHandle");
    let state_handle = StateHandle::new(StateConfig {
        erase_storage_on_start: false,
        blocks_to_retain_in_memory: sequencer_config.blocks_to_retain_in_memory,
        rocks_db_path: sequencer_config.rocks_db_path.clone(),
    });

    tracing::info!("Initializing RepositoryManager");
    let repositories = RepositoryManager::new(
        sequencer_config.blocks_to_retain_in_memory,
        sequencer_config.rocks_db_path.join(REPOSITORY_DB_NAME),
    );
    let proof_storage_db = RocksDB::<ProofColumnFamily>::new(
        &sequencer_config.rocks_db_path.join(PROOF_STORAGE_DB_NAME),
    )
    .expect("Failed to open ProofStorageDB");

    tracing::info!("Initializing ProofStorage");
    let proof_storage = ProofStorage::new(proof_storage_db);

    tracing::info!("Initializing mempools");
    let (l1_mempool, l2_mempool) = zksync_os_mempool::in_memory(
        ZkClient::new(repositories.clone(), state_handle.clone()),
        mempool_config.max_tx_input_bytes,
    );

    tracing::info!("Initializing TreeManager");
    let tree_wrapper = TreeManager::tree_wrapper(Path::new(
        &sequencer_config.rocks_db_path.join(TREE_DB_NAME),
    ));
    let (tree_manager, persistent_tree) = TreeManager::new(tree_wrapper.clone(), tree_receiver);

    tracing::info!("Initializing L1Watcher");
    let l1_watcher = L1Watcher::new(l1_watcher_config, l1_mempool.clone()).await;
    let l1_watcher_task: BoxFuture<anyhow::Result<()>> = match l1_watcher {
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

    // =========== Recover block number to start from and assert that it's consistent with other components ===========

    // this will be the starting block
    let storage_map_compacted_block = state_handle.compacted_block_number();

    // only reading these for assertions
    let repositories_persisted_block = repositories.get_latest_block();
    let wal_block = block_replay_storage.latest_block().unwrap_or(0);
    let tree_last_processed_block = tree_manager
        .last_processed_block()
        .expect("cannot read tree last processed block after initialization");

    tracing::info!(
        storage_map_block = storage_map_compacted_block,
        wal_block = wal_block,
        canonized_block = repositories.get_latest_block(),
        tree_last_processed_block = tree_last_processed_block,
        "▶ Sequencer will start from block {}",
        storage_map_compacted_block + 1
    );

    let starting_block = storage_map_compacted_block + 1;

    assert!(
        wal_block >= storage_map_compacted_block
            && repositories_persisted_block >= storage_map_compacted_block
            && tree_last_processed_block >= storage_map_compacted_block
            && tree_last_processed_block <= wal_block
            && repositories_persisted_block <= wal_block,
        "Block numbers are inconsistent on startup!"
    );

    // ========== Initialize BlockContextProvider and its state ===========
    tracing::info!("Initializing BlockContextProvider");

    let first_replay_record = block_replay_storage.get_replay_record(starting_block);
    assert!(
        first_replay_record.is_some() || starting_block == 1,
        "Unless it's a new chain, replay record must exist"
    );

    let next_l1_priority_id = first_replay_record
        .as_ref()
        .map_or(0, |record| record.starting_l1_priority_id);
    let block_hashes_for_next_block = first_replay_record
        .as_ref()
        .map(|record| record.block_context.block_hashes)
        .unwrap_or_default(); // TODO: take into account genesis block hash.
    let command_block_context_provider = BlockContextProvider::new(
        next_l1_priority_id,
        l1_mempool,
        l2_mempool.clone(),
        block_hashes_for_next_block,
    );

    if !batcher_config.subsystem_enabled {
        tracing::error!(
            "!!! Batcher subsystem disabled via configuration. This mode is only recommended for running tree loadtest."
        );
        unimplemented!("Running without batcher is not supported at the moment.");
        // let mut blocks_receiver = blocks_for_batcher_receiver;
        // async move {
        //     while blocks_receiver.recv().await.is_some() {
        //         // Drop messages silently to prevent backpressure
        //     }
        //     Ok::<(), anyhow::Error>(())
        // }
        // .boxed()
    }
    tracing::info!("Initializing batcher subsystem");
    // ========== Initialize L1 sender ===========
    tracing::info!("Initializing L1 sender");
    let (l1_sender, l1_sender_handle, last_committed_batch_number): (
        L1Sender,
        L1SenderHandle,
        u64,
    ) = L1Sender::new(l1_sender_config).await.expect(
        "Failed to initialize L1Sender. Consider disabling batcher subsystem via configuration.",
    );

    // todo: this will not hold when we do proper batching,
    //  but both number are needed to initialize the batcher
    //  (it needs to know until what block to skip, and it also needs to know the batch number to start with)
    //  potential solution: modify batcher persistence to also store block range per batch - and use it to recover that block number
    let last_committed_block_number = last_committed_batch_number;
    let last_stored_batch_with_proof = proof_storage.latest_stored_batch_number().unwrap_or(0);

    tracing::info!(
        last_committed_batch_number,
        last_committed_block_number,
        last_stored_batch_with_proof,
        "L1 sender initialized"
    );
    assert!(
        last_committed_block_number <= wal_block
            && last_committed_block_number >= storage_map_compacted_block
            && last_stored_batch_with_proof >= last_committed_batch_number,
        "L1 sender last committed block number is inconsistent with WAL or storage map"
    );

    // ========== Initialize Batcher ===========
    tracing::info!("Initializing Batcher");
    let batcher_task = {
        let batcher = Batcher::new(
            last_committed_batch_number + 1,
            sequencer_config.rocks_db_path.clone(),
            blocks_for_batcher_receiver,
            batch_replay_data_sender,
            persistent_tree.clone(),
        );
        Box::pin(batcher.run_loop())
    };

    tracing::info!("Initializing ProverInputGenerator");
    let prover_input_generator_task = {
        let prover_input_generator = ProverInputGenerator::new(
            prover_input_generator_config.logging_enabled,
            prover_input_generator_config.maximum_in_flight_blocks,
            batch_replay_data_receiver,
            batch_for_proving_sender,
            persistent_tree,
            state_handle.clone(),
        );
        Box::pin(prover_input_generator.run_loop())
    };

    // ======= Initialize Prover Api Server========

    let fri_prover_job_manager = Arc::new(ProverJobManager::new(
        batch_for_prover_receiver,
        batch_with_proof_sender,
        prover_api_config.job_timeout,
    ));

    let prover_gapless_committer = GaplessCommitter::new(
        last_committed_batch_number + 1,
        batch_with_proof_receiver,
        proof_storage.clone(),
        l1_sender_handle,
    );

    let prover_server_task = Box::pin(prover_server::run(
        fri_prover_job_manager.clone(),
        proof_storage,
        prover_api_config.address,
    ));

    let fake_provers_task_optional: BoxFuture<anyhow::Result<()>> =
        if prover_api_config.fake_provers.enabled {
            tracing::info!(
                workers = prover_api_config.fake_provers.workers,
                compute_time = ?prover_api_config.fake_provers.compute_time,
                min_task_age = ?prover_api_config.fake_provers.min_age,
                "Initializing FakeProversPool"
            );
            let fake_provers_pool = FakeProversPool::new(
                fri_prover_job_manager.clone(),
                prover_api_config.fake_provers.workers,
                prover_api_config.fake_provers.compute_time,
                prover_api_config.fake_provers.min_age,
            );
            Box::pin(fake_provers_pool.run())
        } else {
            // noop task
            let mut stop_receiver = stop_receiver.clone();
            Box::pin(async move {
                // Defer until we receive stop signal, i.e. a task that does nothing
                stop_receiver
                    .changed()
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
            })
        };

    // ======= Run tasks ===========

    tokio::select! {
        // ── Sequencer task ───────────────────────────────────────────────
        res = run_sequencer_actor(
            starting_block,
            blocks_for_batcher_sender,
            tree_sender,
            command_block_context_provider,
            state_handle.clone(),
            block_replay_storage.clone(),
            repositories.clone(),
            sequencer_config
        ) => {
            match res {
                Ok(_)  => tracing::warn!("Sequencer server unexpectedly exited"),
                Err(e) => tracing::error!("Sequencer server failed: {e:#}"),
            }
        }

        // todo: only start after the sequencer caught up?
        res = run_jsonrpsee_server(
            rpc_config,
            repositories.clone(),
            state_handle.clone(),
            l2_mempool,
            block_replay_storage.clone()
        ) => {
            match res {
                Ok(_)  => tracing::warn!("JSON-RPC server unexpectedly exited"),
                Err(e) => tracing::error!("JSON-RPC server failed: {e:#}"),
            }
        }

        res = tree_manager.run_loop() => {
            match res {
                Ok(_)  => tracing::warn!("TREE server unexpectedly exited"),
                Err(e) => tracing::error!("TREE server failed: {e:#}"),
            }
        }

        res = l1_watcher_task => {
            match res {
                Ok(_)  => tracing::warn!("L1 watcher unexpectedly exited"),
                Err(e) => tracing::error!("L1 watcher failed: {e:#}"),
            }
        }

        // == batcher tasks ==
        res = batcher_task => {
            match res {
                Ok(_)  => tracing::warn!("Batcher task exited"),
                Err(e) => tracing::error!("Batcher task failed: {e:#}"),
            }
        }
        res = prover_input_generator_task => {
            match res {
                Ok(_)  => tracing::warn!("ProverInputGenerator task exited"),
                Err(e) => tracing::error!("ProverInputGenerator task failed: {e:#}"),
            }
        }

        res = prover_server_task => {
            match res {
                Ok(_)  => tracing::warn!("prover_server_job task exited"),
                Err(e) => tracing::error!("prover_server_job task failed: {e:#}"),
            }
        }

        res = prover_gapless_committer.run() => {
            match res {
                Ok(_)  => tracing::warn!("prover_gapless_committer task exited"),
                Err(e) => tracing::error!("prover_gapless_committer task failed: {e:#}"),
            }
        }

        res = fake_provers_task_optional => {
            match res {
                Ok(_)  => tracing::warn!("fake_provers_task_optional task exited"),
                Err(e) => tracing::error!("fake_provers_task_optional task failed: {e:#}"),
            }
        }

        res = l1_sender.run() => {
            match res {
                Ok(_)  => tracing::warn!("L1 sender unexpectedly exited"),
                Err(e) => tracing::error!("L1 sender failed: {e:#}"),
            }
        }

        _ = state_handle.collect_state_metrics(Duration::from_secs(2)) => {
            tracing::warn!("collect_state_metrics unexpectedly exited")
        }
        _ = state_handle.compact_periodically(Duration::from_millis(100)) => {
            tracing::warn!("compact_periodically unexpectedly exited")
        }
        _ = repositories.run_persist_loop() => {
            tracing::warn!("repositories.run_persist_loop() unexpectedly exited")
        }
    }
}
