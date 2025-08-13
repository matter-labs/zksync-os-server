#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

mod batch_sink;
pub mod batcher;
pub mod block_replay_storage;
pub mod config;
pub mod execution;
mod genesis;
mod metadata;
pub mod model;
pub mod prover_api;
mod prover_input_generator;
pub mod reth_state;
pub mod tree_manager;
mod util;

use crate::batch_sink::BatchSink;
use crate::batcher::{Batcher, util::genesis_stored_batch_info};
use crate::block_replay_storage::{BlockReplayColumnFamily, BlockReplayStorage};
use crate::config::{
    BatcherConfig, GenesisConfig, MempoolConfig, ProverApiConfig, ProverInputGeneratorConfig,
    RpcConfig, SequencerConfig,
};
use crate::execution::block_context_provider::BlockContextProvider;
use crate::execution::block_executor::execute_block;
use crate::execution::metrics::EXECUTION_METRICS;
use crate::execution::utils::save_dump;
use crate::genesis::build_genesis;
use crate::metadata::NODE_VERSION;
use crate::prover_api::fake_fri_provers_pool::FakeFriProversPool;
use crate::prover_api::fri_job_manager::FriJobManager;
use crate::prover_api::gapless_committer::GaplessCommitter;
use crate::prover_api::proof_storage::{ProofColumnFamily, ProofStorage};
use crate::prover_api::prover_server;
use crate::prover_api::snark_job_manager::{FakeSnarkProver, SnarkJobManager};
use crate::prover_input_generator::ProverInputGenerator;
use crate::reth_state::ZkClient;
use crate::tree_manager::TreeManager;
use crate::util::peekable_receiver::PeekableReceiver;
use alloy::providers::{DynProvider, ProviderBuilder};
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use futures::stream::{BoxStream, StreamExt};
use model::blocks::{BlockCommand, ProduceCommand};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::time::Instant;
use zk_os_forward_system::run::BlockOutput;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof, ProverInput};
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_l1_sender::l1_discovery::{L1State, get_l1_state};
use zksync_os_l1_sender::run_l1_sender;
use zksync_os_l1_watcher::{L1CommitWatcher, L1TxWatcher, L1WatcherConfig};
use zksync_os_priority_tree::PriorityTreeManager;
use zksync_os_rpc::run_jsonrpsee_server;
use zksync_os_state::{StateConfig, StateHandle};
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::{ReadReplay, ReplayRecord};
use zksync_storage::RocksDB;

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const TREE_DB_NAME: &str = "tree";
const PROOF_STORAGE_DB_NAME: &str = "proofs";
const REPOSITORY_DB_NAME: &str = "repository";

#[allow(clippy::too_many_arguments)]
pub async fn run_sequencer_actor(
    starting_block: u64,

    prover_input_generator_sink: Sender<(BlockOutput, ReplayRecord)>,
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

    while let Some(cmd) = stream.next().await {
        // todo: also report full latency between command invocations
        let block_number = cmd.block_number();

        tracing::info!(
            block_number,
            cmd = cmd.to_string(),
            "▶ starting command. Turning into PreparedCommand.."
        );
        let stage_started_at = Instant::now();

        let prepared_command = command_block_context_provider.prepare_command(cmd).await?;

        tracing::debug!(
            block_number,
            starting_l1_priority_id = prepared_command.starting_l1_priority_id,
            "▶ Prepared command in {:?}. Executing..",
            stage_started_at.elapsed()
        );
        let stage_started_at = Instant::now();

        let (block_output, replay_record, purged_txs) =
            execute_block(prepared_command, state.clone())
                .await
                .map_err(|dump| {
                    let error = anyhow::anyhow!("{}", dump.error);
                    tracing::info!("Saving dump..");
                    if let Err(err) = save_dump(sequencer_config.block_dump_path.clone(), dump) {
                        tracing::error!("Failed to write dump: {err}");
                    }
                    error
                })
                .context("execute_block")?;

        tracing::info!(
            block_number,
            transactions = replay_record.transactions.len(),
            preimages = block_output.published_preimages.len(),
            storage_writes = block_output.storage_writes.len(),
            "▶ Executed after {:?}. Adding to state...",
            stage_started_at.elapsed()
        );

        let stage_started_at = Instant::now();

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
        let stage_started_at = Instant::now();

        // todo: do not call if api is not enabled.
        repositories
            .populate_in_memory_blocking(block_output.clone(), replay_record.transactions.clone())
            .await;

        tracing::debug!(
            block_number,
            "▶ Added to repos in {:?}. Reporting back to block_transaction_provider (mempools)...",
            stage_started_at.elapsed()
        );

        let stage_started_at = Instant::now();

        // TODO: would updating mempool in parallel with state make sense?
        command_block_context_provider.on_canonical_state_change(&block_output, &replay_record);
        let purged_txs_hashes = purged_txs.into_iter().map(|(hash, _)| hash).collect();
        command_block_context_provider.remove_txs(purged_txs_hashes);

        tracing::debug!(
            block_number,
            "▶ Reported to block_transaction_provider in {:?}. Adding to wal...",
            stage_started_at.elapsed()
        );

        let stage_started_at = Instant::now();

        wal.append_replay(replay_record.clone());

        tracing::debug!(
            block_number,
            "▶ Added to wal and canonized in {:?}. Sending to sinks...",
            stage_started_at.elapsed()
        );
        let stage_started_at = Instant::now();

        prover_input_generator_sink
            .send((block_output.clone(), replay_record))
            .await?;
        tree_sink.send(block_output).await?;

        tracing::info!(
            block_number,
            "✔ sent to sinks in {:?}",
            stage_started_at.elapsed()
        );

        EXECUTION_METRICS.block_number[&"execute"].set(block_number);
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
    genesis_config: GenesisConfig,
    rpc_config: RpcConfig,
    mempool_config: MempoolConfig,
    sequencer_config: SequencerConfig,
    l1_sender_config: L1SenderConfig,
    l1_watcher_config: L1WatcherConfig,
    batcher_config: BatcherConfig,
    prover_input_generator_config: ProverInputGeneratorConfig,
    prover_api_config: ProverApiConfig,
) {
    let node_version: semver::Version = NODE_VERSION.parse().unwrap();

    // =========== Boilerplate - initialize components that don't need state recovery or channels ===========
    tracing::info!("Initializing BlockReplayStorage");
    let block_replay_storage_rocks_db = RocksDB::<BlockReplayColumnFamily>::new(
        &sequencer_config
            .rocks_db_path
            .join(BLOCK_REPLAY_WAL_DB_NAME),
    )
    .expect("Failed to open BlockReplayWAL")
    .with_sync_writes();
    let block_replay_storage = BlockReplayStorage::new(
        block_replay_storage_rocks_db,
        genesis_config.chain_id,
        node_version.clone(),
    );

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
        build_genesis(),
    );
    let proof_storage_db = RocksDB::<ProofColumnFamily>::new(
        &sequencer_config.rocks_db_path.join(PROOF_STORAGE_DB_NAME),
    )
    .expect("Failed to open ProofStorageDB");

    tracing::info!("Initializing ProofStorage");
    let proof_storage = ProofStorage::new(proof_storage_db);

    tracing::info!("Initializing mempools");
    let l2_mempool = zksync_os_mempool::in_memory(
        ZkClient::new(
            repositories.clone(),
            state_handle.clone(),
            genesis_config.chain_id,
        ),
        mempool_config.max_tx_input_bytes,
    );

    tracing::info!("reading L1 state");
    let l1_provider = DynProvider::new(
        ProviderBuilder::new()
            .connect(&l1_sender_config.l1_api_url)
            .await
            .expect("failed to connect to L1 api"),
    );
    let l1_state = get_l1_state(
        &l1_provider,
        l1_sender_config.clone(),
        genesis_config.chain_id,
    )
    .await
    .expect("Failed to read L1 state");

    // ======= Initialize async channels  ===========

    // Channel between `BlockExecutor` and `ProverInputGenerator`
    let (blocks_for_prover_input_generator_sender, blocks_for_prover_input_generator_receiver) =
        tokio::sync::mpsc::channel::<(BlockOutput, ReplayRecord)>(10);

    // Channel between `BlockExecutor` and `TreeManager`
    let (tree_sender, tree_receiver) = tokio::sync::mpsc::channel::<BlockOutput>(10);

    // Channel between `ProverInputGenerator` and `Batcher`
    let (blocks_for_batcher_sender, blocks_for_batcher_receiver) =
        tokio::sync::mpsc::channel::<(BlockOutput, ReplayRecord, ProverInput)>(10);

    // Channel between `Batcher` and `ProverAPI`
    let (batch_for_proving_sender, batch_for_prover_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<ProverInput>>(10);

    let (batch_with_proof_sender, batch_with_proof_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<FriProof>>(10);

    // Channel between `L1Watcher` and `BlockContextProvider`
    let (l1_transactions_sender, l1_transactions) = tokio::sync::mpsc::channel(10);

    // Channel between `GaplessCommitter` and `L1Committer`
    let (batch_for_commit_sender, batch_for_commit_receiver) =
        tokio::sync::mpsc::channel::<CommitCommand>(10);

    // Channel between `SnarkJobManager` and `L1ProofSubmitter`
    let (batch_for_l1_proving_sender, batch_for_l1_proving_receiver) =
        tokio::sync::mpsc::channel::<ProofCommand>(10);

    // Channel between `PriorityTree` and `L1Executor`
    let (batch_for_execute_sender, batch_for_execute_receiver) =
        tokio::sync::mpsc::channel::<ExecuteCommand>(10);

    // Channel between `L1Executor` and `BatchSink`
    let (fully_processed_batch_sender, fully_processed_batch_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<FriProof>>(10);

    // There may be batches that are Committed but not Proven on L1 yet, or Proven but not Executed yet.
    // We will reschedule them by loading `BatchEnvelope`s from ProofStorage
    // and sending them to the corresponding channels.
    // Target components don't differentiate whether a batch was rescheduled or loaded during normal operations

    let committed_not_proven_batches = get_committed_not_proven_batches(&l1_state, &proof_storage)
        .await
        .expect("Cannot get committed not proven batches");

    let proven_not_executed_batches = get_proven_not_executed_batches(&l1_state, &proof_storage)
        .await
        .expect("Cannot get proven not executed batches");

    // We need to adopt capacity in accordance to the number of batches that we need to reschedule.
    // Otherwise it's possible that not all rescheduled batches fit into the channel.
    // todo: This may theoretical grow every time node restarts.
    //  Alternatively, we can start processing messages from these channels in parallel with rescheduling -
    //  but then we should defer launching real senders before all pending are processed

    // Channel between `L1Committer` and `SnarkJobManager`
    let (batch_for_snark_sender, batch_for_snark_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<FriProof>>(
            committed_not_proven_batches.len().max(10),
        );

    // Channel between `L1ProofSubmitter` and `PriorityTree`
    let (batch_for_priority_tree_sender, batch_for_priority_tree_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<FriProof>>(
            proven_not_executed_batches.len().max(10),
        );

    reschedule_committed_not_proved_batches(committed_not_proven_batches, &batch_for_snark_sender)
        .await
        .expect("reschedule not proven batches");
    reschedule_proven_not_executed_batches(
        proven_not_executed_batches,
        &batch_for_priority_tree_sender,
    )
    .await
    .expect("reschedule not executed batches");

    // =========== Initialize TreeManager ========
    tracing::info!("Initializing TreeManager");
    let tree_wrapper = TreeManager::tree_wrapper(Path::new(
        &sequencer_config.rocks_db_path.join(TREE_DB_NAME),
    ));
    let (tree_manager, persistent_tree) = TreeManager::new(tree_wrapper.clone(), tree_receiver);

    // =========== Recover block number to start from and assert that it's consistent with other components ===========

    // this will be the starting block
    let storage_map_compacted_block = state_handle.compacted_block_number();

    // only reading these for assertions
    let repositories_persisted_block = repositories.get_latest_persisted_block();
    let wal_block = block_replay_storage.latest_block().unwrap_or(0);
    let tree_last_processed_block = tree_manager
        .last_processed_block()
        .expect("cannot read tree last processed block after initialization");

    tracing::info!(
        storage_map_block = storage_map_compacted_block,
        wal_block = wal_block,
        canonized_block = repositories.get_latest_persisted_block(),
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

    tracing::info!("Initializing L1Watcher");

    let first_replay_record = block_replay_storage.get_replay_record(starting_block);
    assert!(
        first_replay_record.is_some() || starting_block == 1,
        "Unless it's a new chain, replay record must exist"
    );

    let next_l1_priority_id = first_replay_record
        .as_ref()
        .map_or(0, |record| record.starting_l1_priority_id);

    let l1_tx_watcher = L1TxWatcher::new(
        l1_watcher_config.clone(),
        l1_provider.clone(),
        l1_state.diamond_proxy,
        l1_transactions_sender,
        next_l1_priority_id,
    )
    .await;
    let l1_tx_watcher_task = l1_tx_watcher
        .expect("failed to start L1 transaction watcher")
        .run();

    let l1_commit_watcher =
        L1CommitWatcher::new(l1_watcher_config, l1_provider, l1_state.diamond_proxy, 1).await;
    let l1_commit_watcher_task = l1_commit_watcher
        .expect("failed to start L1 commit watcher")
        .run();

    // ========== Initialize BlockContextProvider and its state ===========
    tracing::info!("Initializing BlockContextProvider");

    let previous_block_timestamp: u64 = first_replay_record
        .as_ref()
        .map_or(0, |record| record.block_context.timestamp); // if no previous block, assume genesis block

    let block_hashes_for_next_block = first_replay_record
        .as_ref()
        .map(|record| record.block_context.block_hashes)
        .unwrap_or_default(); // TODO: take into account genesis block hash.
    let command_block_context_provider = BlockContextProvider::new(
        next_l1_priority_id,
        l1_transactions,
        l2_mempool.clone(),
        block_hashes_for_next_block,
        previous_block_timestamp,
        genesis_config.chain_id,
        node_version,
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

    let (last_committed_block_number, prev_batch_info) = if l1_state.last_committed_batch == 0 {
        (0, genesis_stored_batch_info())
    } else {
        let batch_metadata = proof_storage
            .get(l1_state.last_committed_batch)
            .expect("Failed to get last committed block from proof storage")
            .map(|proof| proof.batch)
            .expect("Committed batch is not present in proof storage");
        (
            batch_metadata.last_block_number,
            batch_metadata.commit_batch_info.into(),
        )
    };

    let last_stored_batch_with_proof = proof_storage.latest_stored_batch_number().unwrap_or(0);

    tracing::info!(
        l1_state.last_committed_batch,
        last_committed_block_number,
        last_stored_batch_with_proof,
        "L1 sender initialized"
    );
    assert!(
        last_committed_block_number <= wal_block
            && last_committed_block_number >= storage_map_compacted_block
            && last_stored_batch_with_proof >= l1_state.last_committed_batch,
        "L1 sender last committed block number is inconsistent with WAL or storage map"
    );

    // ========== Initialize ProverInputGenerator ===========
    tracing::info!("Initializing ProverInputGenerator");
    let prover_input_generator_task = {
        let prover_input_generator = ProverInputGenerator::new(
            prover_input_generator_config.logging_enabled,
            prover_input_generator_config.maximum_in_flight_blocks,
            blocks_for_prover_input_generator_receiver,
            blocks_for_batcher_sender,
            persistent_tree.clone(),
            state_handle.clone(),
        );
        Box::pin(prover_input_generator.run_loop())
    };

    // ========== Initialize Batcher ===========
    tracing::info!("Initializing Batcher");
    let batcher_task = {
        let batcher = Batcher::new(
            genesis_config.chain_id,
            last_committed_block_number + 1,
            repositories_persisted_block,
            batcher_config,
            blocks_for_batcher_receiver,
            batch_for_proving_sender,
            persistent_tree,
        );
        Box::pin(batcher.run_loop(prev_batch_info))
    };

    // ======= Initialize Prover Api Server========

    let fri_job_manager = Arc::new(FriJobManager::new(
        batch_for_prover_receiver,
        batch_with_proof_sender,
        prover_api_config.job_timeout,
        prover_api_config.max_assigned_batch_range,
    ));

    let snark_job_manager = Arc::new(SnarkJobManager::new(
        PeekableReceiver::new(batch_for_snark_receiver),
        batch_for_l1_proving_sender,
        prover_api_config.max_fris_per_snark,
    ));

    let prover_gapless_committer = GaplessCommitter::new(
        l1_state.last_committed_batch + 1,
        batch_with_proof_receiver,
        proof_storage.clone(),
        batch_for_commit_sender,
    );

    let prover_server_task = Box::pin(prover_server::run(
        fri_job_manager.clone(),
        snark_job_manager.clone(),
        proof_storage.clone(),
        prover_api_config.address,
    ));

    let fake_fri_provers_task_optional: BoxFuture<Result<()>> =
        if prover_api_config.fake_fri_provers.enabled {
            tracing::info!(
                workers = prover_api_config.fake_fri_provers.workers,
                compute_time = ?prover_api_config.fake_fri_provers.compute_time,
                min_task_age = ?prover_api_config.fake_fri_provers.min_age,
                "Initializing fake FRI provers"
            );
            let fake_provers_pool = FakeFriProversPool::new(
                fri_job_manager.clone(),
                prover_api_config.fake_fri_provers.workers,
                prover_api_config.fake_fri_provers.compute_time,
                prover_api_config.fake_fri_provers.min_age,
            );
            Box::pin(fake_provers_pool.run())
        } else {
            noop_task(stop_receiver.clone())
        };

    let fake_snark_provers_task_optional: BoxFuture<Result<()>> =
        if prover_api_config.fake_snark_provers.enabled {
            tracing::info!(
                max_batch_age = ?prover_api_config.fake_snark_provers.max_batch_age,
                "Initializing fake SNARK prover"
            );
            let fake_provers_pool = FakeSnarkProver::new(
                snark_job_manager.clone(),
                prover_api_config.fake_snark_provers.max_batch_age,
            );
            Box::pin(fake_provers_pool.run())
        } else {
            noop_task(stop_receiver)
        };

    let last_executed_block = proof_storage
        .get(l1_state.last_executed_batch)
        .expect("failed to load last executed batch")
        .map(|batch_envelope| batch_envelope.batch.last_block_number)
        .unwrap_or(0);
    let (l1_committer, l1_proof_submitter, l1_executor) = run_l1_senders(
        l1_sender_config,
        batch_for_commit_receiver,
        batch_for_snark_sender,
        batch_for_l1_proving_receiver,
        batch_for_priority_tree_sender,
        batch_for_execute_receiver,
        fully_processed_batch_sender,
        &l1_state,
    );

    let priority_tree_manager = PriorityTreeManager::new(
        block_replay_storage.clone(),
        last_executed_block,
        batch_for_priority_tree_receiver,
        batch_for_execute_sender,
    )
    .unwrap();

    let batch_sink = BatchSink::new(fully_processed_batch_receiver);

    // ======= Run tasks ===========

    tokio::select! {
        // ── Sequencer task ───────────────────────────────────────────────
        res = run_sequencer_actor(
            starting_block,
            blocks_for_prover_input_generator_sender,
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
            genesis_config.chain_id,
            l1_state.bridgehub,
            repositories.clone(),
            block_replay_storage.clone(),
            state_handle.clone(),
            l2_mempool,
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

        res = l1_tx_watcher_task => {
            match res {
                Ok(_)  => tracing::warn!("L1 transaction watcher unexpectedly exited"),
                Err(e) => tracing::error!("L1 transaction watcher failed: {e:#}"),
            }
        }

        res = l1_commit_watcher_task => {
            match res {
                Ok(_)  => tracing::warn!("L1 commit watcher unexpectedly exited"),
                Err(e) => tracing::error!("L1 commit watcher failed: {e:#}"),
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

        res = fake_fri_provers_task_optional => {
            match res {
                Ok(_)  => tracing::warn!("fake_provers_task_optional task exited"),
                Err(e) => tracing::error!("fake_provers_task_optional task failed: {e:#}"),
            }
        }

        res = fake_snark_provers_task_optional => {
            match res {
                Ok(_)  => tracing::warn!("fake_provers_task_optional task exited"),
                Err(e) => tracing::error!("fake_provers_task_optional task failed: {e:#}"),
            }
        }

        res = l1_committer => {
            match res {
                Ok(_)  => tracing::warn!("L1 committer unexpectedly exited"),
                Err(e) => tracing::error!("L1 committer failed: {e:#}"),
            }
        }

        res = l1_proof_submitter => {
            match res {
                Ok(_)  => tracing::warn!("L1 proof submitter unexpectedly exited"),
                Err(e) => tracing::error!("L1 proof submitter failed: {e:#}"),
            }
        }

        res = l1_executor => {
            match res {
                Ok(_)  => tracing::warn!("L1 executor unexpectedly exited"),
                Err(e) => tracing::error!("L1 executor failed: {e:#}"),
            }
        }

        res = priority_tree_manager.run() => {
            match res {
                Ok(_)  => tracing::warn!("Priority tree manager unexpectedly exited"),
                Err(e) => tracing::error!("Priority tree manager failed: {e:#}"),
            }
        }

        res = batch_sink.run() => {
            match res {
                Ok(_)  => tracing::warn!("batch_sink task exited"),
                Err(e) => tracing::error!("batch_sink task failed: {e:#}"),
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

fn noop_task(stop_receiver: watch::Receiver<bool>) -> Pin<Box<impl Future<Output = Result<()>>>> {
    // noop task
    let mut stop_receiver = stop_receiver.clone();
    Box::pin(async move {
        // Defer until we receive stop signal, i.e. a task that does nothing
        stop_receiver
            .changed()
            .await
            .map_err(|e| anyhow::anyhow!(e))
    })
}

#[allow(clippy::too_many_arguments)]
fn run_l1_senders(
    l1_sender_config: L1SenderConfig,

    batch_for_commit_receiver: Receiver<CommitCommand>,
    batch_for_snark_sender: Sender<BatchEnvelope<FriProof>>,

    batch_for_l1_proving_receiver: Receiver<ProofCommand>,
    batch_for_priority_tree_sender: Sender<BatchEnvelope<FriProof>>,

    batch_for_execute_receiver: Receiver<ExecuteCommand>,
    fully_processed_batch_sender: Sender<BatchEnvelope<FriProof>>,

    l1_state: &L1State,
) -> (
    impl Future<Output = Result<()>>,
    impl Future<Output = Result<()>>,
    impl Future<Output = Result<()>>,
) {
    let l1_committer = run_l1_sender(
        batch_for_commit_receiver,
        batch_for_snark_sender,
        l1_state.validator_timelock,
        l1_sender_config.operator_commit_pk.clone(),
        l1_sender_config.l1_api_url.clone(),
        l1_sender_config.max_fee_per_gas(),
        l1_sender_config.max_priority_fee_per_gas(),
        l1_sender_config.command_limit,
    );

    let l1_proof_submitter = run_l1_sender(
        batch_for_l1_proving_receiver,
        batch_for_priority_tree_sender,
        l1_state.diamond_proxy,
        l1_sender_config.operator_prove_pk.clone(),
        l1_sender_config.l1_api_url.clone(),
        l1_sender_config.max_fee_per_gas(),
        l1_sender_config.max_priority_fee_per_gas(),
        l1_sender_config.command_limit,
    );

    let l1_executor = run_l1_sender(
        batch_for_execute_receiver,
        fully_processed_batch_sender,
        l1_state.diamond_proxy,
        l1_sender_config.operator_execute_pk.clone(),
        l1_sender_config.l1_api_url.clone(),
        l1_sender_config.max_fee_per_gas(),
        l1_sender_config.max_priority_fee_per_gas(),
        l1_sender_config.command_limit,
    );
    (l1_committer, l1_proof_submitter, l1_executor)
}

async fn get_committed_not_proven_batches(
    l1_state: &L1State,
    proof_storage: &ProofStorage,
) -> anyhow::Result<Vec<BatchEnvelope<FriProof>>> {
    let mut batch_to_prove = l1_state.last_proved_batch + 1;
    let mut batches_to_reschedule = Vec::new();
    while batch_to_prove <= l1_state.last_committed_batch {
        let batch_with_proof = proof_storage
            .get(batch_to_prove)?
            .context("Failed to get batch")?;
        batches_to_reschedule.push(batch_with_proof);
        batch_to_prove += 1;
    }
    Ok(batches_to_reschedule)
}

pub async fn reschedule_committed_not_proved_batches(
    batches_to_reschedule: Vec<BatchEnvelope<FriProof>>,
    batch_for_snark_sender: &Sender<BatchEnvelope<FriProof>>,
) -> Result<()> {
    if !batches_to_reschedule.is_empty() {
        tracing::info!(
            "Rescheduling batches {} to {} for SNARK proving",
            batches_to_reschedule.first().unwrap().batch_number(),
            batches_to_reschedule.last().unwrap().batch_number(),
        );
        if batches_to_reschedule.len() > batch_for_snark_sender.capacity() {
            tracing::warn!(
                "SNARK prover capacity is too small to handle {} batches",
                batches_to_reschedule.len()
            );
        }
        for batch in batches_to_reschedule {
            batch_for_snark_sender.send(batch).await?
        }
    }

    Ok(())
}

async fn get_proven_not_executed_batches(
    l1_state: &L1State,
    proof_storage: &ProofStorage,
) -> Result<Vec<BatchEnvelope<FriProof>>> {
    let mut batch_to_execute = l1_state.last_executed_batch + 1;
    let mut batches_to_reschedule = Vec::new();
    while batch_to_execute <= l1_state.last_proved_batch {
        let batch_with_proof = proof_storage
            .get(batch_to_execute)?
            .context("Failed to get batch")?;
        batches_to_reschedule.push(batch_with_proof);
        batch_to_execute += 1;
    }
    Ok(batches_to_reschedule)
}

pub async fn reschedule_proven_not_executed_batches(
    batches_to_reschedule: Vec<BatchEnvelope<FriProof>>,
    batch_for_priority_tree_sender: &Sender<BatchEnvelope<FriProof>>,
) -> anyhow::Result<()> {
    if !batches_to_reschedule.is_empty() {
        tracing::info!(
            "Rescheduling batches {} to {} for execution",
            batches_to_reschedule.first().unwrap().batch_number(),
            batches_to_reschedule.last().unwrap().batch_number(),
        );
        for batch in batches_to_reschedule {
            batch_for_priority_tree_sender.send(batch).await?
        }
    }

    Ok(())
}
