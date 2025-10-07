#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]
mod batch_sink;
pub mod batcher;
pub mod block_replay_storage;
mod command_source;
pub mod config;
mod en_remote_config;
mod l1_provider;
mod metadata;
mod node_state_on_startup;
mod noop_l1_sender;
pub mod prover_api;
mod prover_input_generator;
mod replay_transport;
pub mod reth_state;
pub mod sentry;
mod state_initializer;
pub mod tree_manager;
mod util;
pub mod zkstack_config;

use crate::batch_sink::BatchSink;
use crate::batcher::{Batcher, util::load_genesis_stored_batch_info};
use crate::block_replay_storage::BlockReplayStorage;
use crate::command_source::{ExternalNodeCommandSource, MainNodeCommandSource};
use crate::config::{Config, L1SenderConfig, ProverApiConfig};
use crate::en_remote_config::load_remote_config;
use crate::l1_provider::build_node_l1_provider;
use crate::metadata::NODE_VERSION;
use crate::node_state_on_startup::NodeStateOnStartup;
use crate::noop_l1_sender::run_noop_l1_sender;
use crate::prover_api::fake_fri_provers_pool::FakeFriProversPool;
use crate::prover_api::fri_job_manager::FriJobManager;
use crate::prover_api::fri_proving_pipeline_step::FriProvingPipelineStep;
use crate::prover_api::gapless_committer::GaplessCommitter;
use crate::prover_api::proof_storage::ProofStorage;
use crate::prover_api::prover_server;
use crate::prover_api::snark_job_manager::{FakeSnarkProver, SnarkJobManager};
use crate::prover_input_generator::ProverInputGenerator;
use crate::replay_transport::replay_server;
use crate::reth_state::ZkClient;
use crate::state_initializer::StateInitializer;
use crate::tree_manager::TreeManager;
use alloy::network::EthereumWallet;
use alloy::providers::{Provider, WalletProvider};
use anyhow::{Context, Result};
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::BoxStream;
use ruint::aliases::U256;
use smart_config::value::ExposeSecret;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::task::JoinSet;
use zksync_os_genesis::{FileGenesisInputSource, Genesis, GenesisInputSource};
use zksync_os_interface::types::BlockHashes;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_l1_sender::l1_discovery::{L1State, get_l1_state};
use zksync_os_l1_sender::run_l1_sender;
use zksync_os_l1_watcher::{L1CommitWatcher, L1ExecuteWatcher, L1TxWatcher};
use zksync_os_mempool::RethPool;
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_multivm::LATEST_EXECUTION_VERSION;
use zksync_os_object_store::ObjectStoreFactory;
use zksync_os_observability::GENERAL_METRICS;
use zksync_os_pipeline::PeekableReceiver;
use zksync_os_priority_tree::PriorityTreeManager;
use zksync_os_rpc::{RpcStorage, run_jsonrpsee_server};
use zksync_os_sequencer::execution::Sequencer;
use zksync_os_sequencer::execution::block_context_provider::BlockContextProvider;
use zksync_os_sequencer::model::blocks::{BlockCommand, ProduceCommand};
use zksync_os_status_server::run_status_server;
use zksync_os_storage::in_memory::Finality;
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::{
    FinalityStatus, ReadBatch, ReadFinality, ReadReplay, ReadRepository, ReadStateHistory,
    RepositoryBlock, WriteState,
};

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const STATE_TREE_DB_NAME: &str = "tree";
const PRIORITY_TREE_DB_NAME: &str = "priority_txs_tree";
const REPOSITORY_DB_NAME: &str = "repository";

#[allow(clippy::too_many_arguments)]
pub async fn run<State: ReadStateHistory + WriteState + StateInitializer + Clone>(
    _stop_receiver: watch::Receiver<bool>,
    config: Config,
) {
    let node_version: semver::Version = NODE_VERSION.parse().unwrap();
    let role: &'static str = if config.sequencer_config.is_main_node() {
        "main_node"
    } else {
        "external_node"
    };

    let process_started_at = Instant::now();
    GENERAL_METRICS.process_started_at[&(NODE_VERSION, role)].set(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
    );

    tracing::info!(version = %node_version, role, "Initializing Node");

    let (bridgehub_address, chain_id, genesis_input_source) =
        if config.sequencer_config.is_main_node() {
            let genesis_input_source: Arc<dyn GenesisInputSource> =
                Arc::new(FileGenesisInputSource::new(
                    config
                        .genesis_config
                        .genesis_input_path
                        .clone()
                        .expect("Missing `genesis_input_path`"),
                ));
            (
                config
                    .genesis_config
                    .bridgehub_address
                    .expect("Missing `bridgehub_address`"),
                config.genesis_config.chain_id.expect("Missing `chain_id`"),
                genesis_input_source,
            )
        } else {
            let main_node_rpc_url = config
                .general_config
                .main_node_rpc_url
                .clone()
                .expect("Missing `main_node_rpc_url` in external node config");
            load_remote_config(&main_node_rpc_url, &config.genesis_config)
                .await
                .unwrap()
        };

    // Channel between L1Watcher and Sequencer
    let (l1_transactions_sender, l1_transactions_for_sequencer) = tokio::sync::mpsc::channel(5);

    tracing::info!("Initializing BatchStorage");
    let batch_storage = ProofStorage::new(
        ObjectStoreFactory::new(config.prover_api_config.object_store.clone())
            .create_store()
            .await
            .unwrap(),
    );

    tracing::info!("Initializing BlockReplayStorage");

    let block_replay_storage = BlockReplayStorage::new(
        config.general_config.rocks_db_path.clone(),
        chain_id,
        node_version.clone(),
        LATEST_EXECUTION_VERSION,
    );

    // This is the only place where we initialize L1 provider, every component shares the same
    // cloned provider.
    let l1_provider = build_node_l1_provider(&config.general_config.l1_rpc_url).await;

    tracing::info!("Reading L1 state");
    let mut l1_state = get_l1_state(&l1_provider, bridgehub_address, chain_id)
        .await
        .expect("Failed to read L1 state");
    if config.sequencer_config.is_main_node() {
        // On a main node, we need to wait for the pending L1 transactions (commit/prove/execute) to be mined before proceeding.
        l1_state = l1_state
            .wait_to_finalize(&l1_provider, chain_id)
            .await
            .expect("failed to wait for L1 state finalization");
    }
    tracing::info!(?l1_state, "L1 state");
    l1_state.report_metrics();

    let genesis = Genesis::new(
        genesis_input_source.clone(),
        l1_provider.clone().erased(),
        l1_state.diamond_proxy,
    );

    tracing::info!("Initializing Tree RocksDB");
    let tree_db = TreeManager::load_or_initialize_tree(
        Path::new(&config.general_config.rocks_db_path.join(STATE_TREE_DB_NAME)),
        &genesis,
    )
    .await;

    tracing::info!("Initializing RepositoryManager");
    let repositories = RepositoryManager::new(
        config.general_config.blocks_to_retain_in_memory,
        config.general_config.rocks_db_path.join(REPOSITORY_DB_NAME),
        &genesis,
    )
    .await;

    let state = State::new(&config.general_config, &genesis).await;

    tracing::info!("Initializing mempools");
    let l2_mempool = zksync_os_mempool::in_memory(
        ZkClient::new(repositories.clone(), state.clone(), chain_id),
        config.mempool_config.clone().into(),
        config.tx_validator_config.clone().into(),
    );

    let (last_l1_committed_block, last_l1_proved_block, last_l1_executed_block) =
        commit_proof_execute_block_numbers(&l1_state, &batch_storage).await;

    let node_startup_state = NodeStateOnStartup {
        is_main_node: config.sequencer_config.is_main_node(),
        l1_state,
        state_block_range_available: state.block_range_available(),
        block_replay_storage_last_block: block_replay_storage.latest_block().unwrap_or(0),
        tree_last_block: tree_db
            .latest_version()
            .expect("cannot read tree last processed block after initialization")
            .expect("tree database is not initialized"),
        repositories_persisted_block: repositories.get_latest_block(),
        last_l1_committed_block,
        last_l1_proved_block,
        last_l1_executed_block,
    };

    let desired_starting_block = if let Some(forced_starting_block_number) =
        config.general_config.force_starting_block_number
    {
        forced_starting_block_number
    } else {
        [
            node_startup_state
                .block_replay_storage_last_block
                .saturating_sub(config.general_config.min_blocks_to_replay as u64),
            node_startup_state.last_l1_committed_block + 1,
            node_startup_state.repositories_persisted_block + 1,
            node_startup_state.tree_last_block + 1,
            state.block_range_available().end() + 1,
        ]
        .into_iter()
        .min()
        .unwrap()
    };

    let starting_block = if desired_starting_block < state.block_range_available().start() + 1 {
        tracing::warn!(
            desired_starting_block,
            config.general_config.force_starting_block_number,
            min_block_available_in_state = state.block_range_available().start() + 1,
            "Desired starting block is not available in state. Starting from zero."
        );
        1
    } else {
        desired_starting_block
    };

    tracing::info!(
        config.general_config.min_blocks_to_replay,
        config.general_config.force_starting_block_number,
        ?node_startup_state,
        starting_block,
        "Node state on startup"
    );

    node_startup_state.assert_consistency();

    tracing::info!("Initializing L1 Watchers");
    let finality_storage = Finality::new(FinalityStatus {
        last_committed_block: last_l1_committed_block,
        last_executed_block: last_l1_executed_block,
    });

    let mut tasks: JoinSet<()> = JoinSet::new();
    tasks.spawn(
        L1CommitWatcher::new(
            config.l1_watcher_config.clone().into(),
            l1_provider.clone().erased(),
            node_startup_state.l1_state.diamond_proxy,
            finality_storage.clone(),
            batch_storage.clone(),
        )
        .await
        .expect("failed to start L1 commit watcher")
        .run()
        .map(report_exit("L1 commit watcher")),
    );

    tasks.spawn(
        L1ExecuteWatcher::new(
            config.l1_watcher_config.clone().into(),
            l1_provider.clone().erased(),
            node_startup_state.l1_state.diamond_proxy,
            finality_storage.clone(),
            batch_storage.clone(),
        )
        .await
        .expect("failed to start L1 execute watcher")
        .run()
        .map(report_exit("L1 execute watcher")),
    );

    let first_replay_record = block_replay_storage.get_replay_record(starting_block);
    assert!(
        first_replay_record.is_some() || starting_block == 1,
        "Unless it's a new chain, replay record must exist"
    );

    let next_l1_priority_id = first_replay_record
        .as_ref()
        .map_or(0, |record| record.starting_l1_priority_id);

    tasks.spawn(
        L1TxWatcher::new(
            config.l1_watcher_config.clone().into(),
            l1_provider.clone().erased(),
            node_startup_state.l1_state.diamond_proxy,
            l1_transactions_sender,
            next_l1_priority_id,
        )
        .await
        .expect("failed to start L1 transaction watcher")
        .run()
        .map(report_exit("L1 transaction watcher")),
    );

    // ======== Start Status Server ========
    tasks.spawn(
        run_status_server(
            config.status_server_config.address.clone(),
            _stop_receiver.clone(),
        )
        .map(report_exit("Status server")),
    );

    // =========== Start JSON RPC ========

    let rpc_storage = RpcStorage::new(
        repositories.clone(),
        block_replay_storage.clone(),
        finality_storage.clone(),
        batch_storage.clone(),
        state.clone(),
    );

    tasks.spawn(
        run_jsonrpsee_server(
            config.rpc_config.clone().into(),
            chain_id,
            node_startup_state.l1_state.bridgehub,
            rpc_storage,
            l2_mempool.clone(),
            genesis_input_source,
        )
        .map(report_exit("JSON-RPC server")),
    );

    // ========== Start BlockContextProvider and its state ===========
    tracing::info!("Initializing BlockContextProvider");

    let previous_block_timestamp: u64 = first_replay_record
        .as_ref()
        .map_or(0, |record| record.previous_block_timestamp); // if no previous block, assume genesis block

    let block_hashes_for_next_block = first_replay_record
        .as_ref()
        .map(|record| record.block_context.block_hashes)
        .unwrap_or_else(|| block_hashes_for_first_block(&repositories));

    // todo: `BlockContextProvider` initialization and its dependencies
    // should be moved to `sequencer`. Left here to reduce the PR size.
    let block_context_provider: BlockContextProvider<RethPool<ZkClient<State>>> =
        BlockContextProvider::new(
            next_l1_priority_id,
            l1_transactions_for_sequencer,
            l2_mempool,
            block_hashes_for_next_block,
            previous_block_timestamp,
            chain_id,
            config.sequencer_config.block_gas_limit,
            config.sequencer_config.block_pubdata_limit_bytes,
            node_version,
            genesis,
            config.sequencer_config.fee_collector_address,
        );

    // ========== Start Sequencer ===========
    tasks.spawn(
        replay_server(
            block_replay_storage.clone(),
            config.sequencer_config.block_replay_server_address.clone(),
        )
        .map(report_exit("replay server")),
    );

    let repositories_clone = repositories.clone();
    tasks.spawn(async move {
        repositories_clone
            .run_persist_loop()
            .map(|_| tracing::warn!("repositories.run_persist_loop() unexpectedly exited"))
            .await
    });
    let state_clone = state.clone();
    tasks.spawn(async move {
        state_clone
            .compact_periodically_optional()
            .map(|_| tracing::warn!("state.compact_periodically() unexpectedly exited"))
            .await;
    });

    if config.sequencer_config.is_main_node() {
        // Main Node
        let genesis_block = repositories
            .get_block_by_number(0)
            .expect("Failed to read genesis block from repositories")
            .expect("Missing genesis block in repositories");
        run_main_node_pipeline(
            config,
            l1_provider.clone(),
            genesis_block,
            batch_storage,
            node_startup_state,
            block_replay_storage,
            &mut tasks,
            state,
            starting_block,
            repositories,
            block_context_provider,
            tree_db,
            finality_storage,
            chain_id,
            _stop_receiver.clone(),
        )
        .await;
    } else {
        // External Node
        run_en_pipeline(
            config,
            batch_storage,
            node_startup_state,
            block_replay_storage,
            &mut tasks,
            block_context_provider,
            state,
            tree_db,
            starting_block,
            repositories,
            finality_storage,
            _stop_receiver.clone(),
        )
        .await;
    };
    let startup_time = process_started_at.elapsed();
    GENERAL_METRICS.startup_time[&"total"].set(startup_time.as_secs_f64());
    tracing::info!("All components initialized in {startup_time:?}");
    tasks.join_next().await;
    tracing::info!("One of the subsystems exited - exiting process.");
}

// todo: extract to a `command_source.rs` - left here to reduce PR size.
pub fn command_source(
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
async fn run_main_node_pipeline<
    State: ReadStateHistory + WriteState + StateInitializer + Clone,
    Finality: ReadFinality,
>(
    config: Config,
    l1_provider: impl Provider + WalletProvider<Wallet = EthereumWallet> + Clone + 'static,
    genesis_block: RepositoryBlock,
    batch_storage: ProofStorage,
    node_state_on_startup: NodeStateOnStartup,
    block_replay_storage: BlockReplayStorage,
    tasks: &mut JoinSet<()>,
    state: State,
    starting_block: u64,
    repositories: RepositoryManager,
    block_context_provider: BlockContextProvider<RethPool<ZkClient<State>>>,
    tree: MerkleTree<RocksDBWrapper>,
    finality: Finality,
    chain_id: u64,
    _stop_receiver: watch::Receiver<bool>,
) {
    // NOTE: the execution pipeline is being migrated to the Pipeline Framework.
    // The current state is intermediary.

    // Migrated components:
    // CommandSource -> Sequencer -> TreeManager -> ProverInputGenerator -> Batcher
    // Other components (not migrated yet):
    // -> FriProverManager -> L1Committer -> SnarkProverManager -> L1Prover -> PriorityTree -> L1Executor

    let pipeline = zksync_os_pipeline::Pipeline::new()
        .pipe(MainNodeCommandSource {
            block_replay_storage: block_replay_storage.clone(),
            starting_block,
            block_time: config.sequencer_config.block_time,
            max_transactions_in_block: config.sequencer_config.max_transactions_in_block,
        })
        .pipe(Sequencer {
            block_context_provider,
            state: state.clone(),
            wal: block_replay_storage.clone(),
            repositories: repositories.clone(),
            sequencer_config: config.sequencer_config.clone().into(),
        })
        .pipe(TreeManager { tree: tree.clone() });

    // Channel between `SnarkJobManager` and `L1ProofSubmitter`
    let (batch_for_l1_proving_sender, batch_for_l1_proving_receiver) =
        tokio::sync::mpsc::channel::<ProofCommand>(5);

    // Channel between `PriorityTree` and `L1Executor`
    let (batch_for_execute_sender, batch_for_execute_receiver) =
        tokio::sync::mpsc::channel::<ExecuteCommand>(5);

    // Channel between `PriorityTree` tasks
    let (priority_txs_internal_sender, priority_txs_internal_receiver) =
        tokio::sync::mpsc::channel::<(u64, u64, Option<usize>)>(1000);

    // Channel for `PriorityTree` cache task
    let (executed_batch_numbers_sender, executed_batch_numbers_receiver) =
        tokio::sync::mpsc::channel::<u64>(1000);

    // Channel between `L1Executor` and `BatchSink`
    let (fully_processed_batch_sender, fully_processed_batch_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<FriProof>>(5);

    let last_committed_batch_info = if node_state_on_startup.l1_state.last_committed_batch == 0 {
        load_genesis_stored_batch_info(genesis_block, tree.clone()).await
    } else {
        batch_storage
            .get(node_state_on_startup.l1_state.last_committed_batch)
            .await
            .expect("Failed to get last committed block from proof storage")
            .expect("Committed batch is not present in proof storage")
            .batch
            .commit_batch_info
            .into()
    };

    let committed_batch = batch_storage
        .get(node_state_on_startup.l1_state.last_committed_batch)
        .await
        .expect("failed to load proof for last_committed_batch");
    assert!(
        node_state_on_startup.l1_state.last_committed_batch == 0 || committed_batch.is_some(),
        "Inconsistent batch numbers: committed batch not found in proof storage."
    );

    // There may be batches that are Committed but not Proven on L1 yet, or Proven but not Executed yet.
    // We will reschedule them by loading `BatchEnvelope`s from ProofStorage
    // and sending them to the corresponding channels.
    // Target components don't differentiate whether a batch was rescheduled or loaded during normal operations

    let committed_not_proven_batches =
        get_committed_not_proven_batches(&node_state_on_startup.l1_state, &batch_storage)
            .await
            .expect("Cannot get committed not proven batches");

    let proven_not_executed_batches =
        get_proven_not_executed_batches(&node_state_on_startup.l1_state, &batch_storage)
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

    let batcher_subsystem_first_block_to_process =
        node_state_on_startup.last_l1_committed_block + 1;
    tracing::info!("Initializing Batcher");

    let prover_input_generation_first_block_to_process = if config
        .prover_input_generator_config
        .force_process_old_blocks
    {
        // Prover input generator will (re)process all the blocks replayed by the node on startup - at least `min_blocks_to_replay`.
        // Note that it doesn't always start from zero.
        0
    } else {
        // Prover input generator will skip the blocks that are already FRI proved and committed to L1.
        batcher_subsystem_first_block_to_process
    };

    let pipeline_after_batcher = pipeline
        .pipe(ProverInputGenerator {
            enable_logging: config.prover_input_generator_config.logging_enabled,
            maximum_in_flight_blocks: config
                .prover_input_generator_config
                .maximum_in_flight_blocks,
            first_block_to_process: prover_input_generation_first_block_to_process,
            app_bin_base_path: config
                .prover_input_generator_config
                .app_bin_unpack_path
                .clone(),
            read_state: state.clone(),
        })
        .pipe(Batcher {
            chain_id,
            chain_address: node_state_on_startup.l1_state.diamond_proxy,
            first_block_to_process: batcher_subsystem_first_block_to_process,
            last_persisted_block: node_state_on_startup.repositories_persisted_block,
            pubdata_limit_bytes: config.sequencer_config.block_pubdata_limit_bytes,
            batcher_config: config.batcher_config.clone(),
            prev_batch_info: last_committed_batch_info,
        });

    let (fri_proving_step, fri_job_manager) = FriProvingPipelineStep::new(
        config.prover_api_config.job_timeout,
        config.prover_api_config.max_assigned_batch_range,
    );

    let pipeline_after_gapless =
        pipeline_after_batcher
            .pipe(fri_proving_step)
            .pipe(GaplessCommitter {
                next_expected: node_state_on_startup.l1_state.last_committed_batch + 1,
                proof_storage: batch_storage.clone(),
                da_input_mode: node_state_on_startup.l1_state.da_input_mode,
            });

    let snark_job_manager = Arc::new(SnarkJobManager::new(
        PeekableReceiver::new(batch_for_snark_receiver),
        batch_for_l1_proving_sender,
        config.prover_api_config.max_fris_per_snark,
    ));

    tasks.spawn(
        prover_server::run(
            fri_job_manager.clone(),
            snark_job_manager.clone(),
            batch_storage.clone(),
            config.prover_api_config.address.clone(),
        )
        .map(report_exit("prover_server_job")),
    );

    if config.prover_api_config.fake_fri_provers.enabled {
        run_fake_fri_provers(&config.prover_api_config, tasks, fri_job_manager);
    }

    if config.prover_api_config.fake_snark_provers.enabled {
        run_fake_snark_provers(&config.prover_api_config, tasks, snark_job_manager);
    }

    let batch_for_commit_receiver = pipeline_after_gapless.into_receiver().into_inner();

    if config.l1_sender_config.enabled {
        let (l1_committer, l1_proof_submitter, l1_executor) = run_l1_senders(
            config.l1_sender_config,
            l1_provider,
            batch_for_commit_receiver,
            batch_for_snark_sender,
            batch_for_l1_proving_receiver,
            batch_for_priority_tree_sender,
            batch_for_execute_receiver,
            fully_processed_batch_sender,
            &node_state_on_startup.l1_state,
        );
        tasks.spawn(l1_committer.map(report_exit("L1 committer")));
        tasks.spawn(l1_proof_submitter.map(report_exit("L1 proof submitter")));
        tasks.spawn(l1_executor.map(report_exit("L1 executor")));
    } else {
        tracing::warn!("Starting without L1 senders! `l1_sender_enabled` config is false");
        tasks.spawn(
            run_noop_l1_sender(batch_for_commit_receiver, batch_for_snark_sender)
                .map(report_exit("L1 committer")),
        );
        tasks.spawn(
            run_noop_l1_sender(
                batch_for_l1_proving_receiver,
                batch_for_priority_tree_sender,
            )
            .map(report_exit("L1 committer")),
        );
        tasks.spawn(
            run_noop_l1_sender(batch_for_execute_receiver, fully_processed_batch_sender)
                .map(report_exit("L1 committer")),
        );
    }

    let priority_tree_manager = PriorityTreeManager::new(
        block_replay_storage.clone(),
        node_state_on_startup.last_l1_executed_block,
        Path::new(
            &config
                .general_config
                .rocks_db_path
                .join(PRIORITY_TREE_DB_NAME),
        ),
    )
    .unwrap();
    // Run task that adds new txs to PriorityTree and prepares `ExecuteCommand`s.
    tasks.spawn(
        priority_tree_manager
            .clone()
            .prepare_execute_commands(
                batch_storage.clone(),
                Some(batch_for_priority_tree_receiver),
                None,
                priority_txs_internal_sender,
                Some(batch_for_execute_sender),
            )
            .map(report_exit(
                "priority_tree_manager#prepare_execute_commands",
            )),
    );
    // Auxiliary task that yields a channel of executed batch numbers.
    tasks.spawn(
        util::finalized_block_channel::send_executed_and_replayed_batch_numbers(
            executed_batch_numbers_sender,
            batch_storage.clone(),
            finality,
            None,
            node_state_on_startup.l1_state.last_executed_batch + 1,
        )
        .map(report_exit("send_executed_batch_numbers")),
    );
    // Run task that persists PriorityTree for executed batches and drops old data.
    tasks.spawn(
        priority_tree_manager
            .keep_caching(
                executed_batch_numbers_receiver,
                priority_txs_internal_receiver,
            )
            .map(report_exit("priority_tree_manager#keep_caching")),
    );

    tasks.spawn(
        BatchSink::new(fully_processed_batch_receiver)
            .run()
            .map(report_exit("batch_sink")),
    );
}

/// Only for EN - we still populate channels destined for the batcher subsystem -
/// need to drain them to not get stuck
#[allow(clippy::too_many_arguments)]
async fn run_en_pipeline<
    State: ReadStateHistory + WriteState + StateInitializer + Clone,
    Finality: ReadFinality + Clone,
>(
    config: Config,
    batch_storage: ProofStorage,
    node_state_on_startup: NodeStateOnStartup,
    block_replay_storage: BlockReplayStorage,
    tasks: &mut JoinSet<()>,
    block_context_provider: BlockContextProvider<RethPool<ZkClient<State>>>,
    state: State,
    tree: MerkleTree<RocksDBWrapper>,
    starting_block: u64,
    repositories: RepositoryManager,
    finality: Finality,
    _stop_receiver: watch::Receiver<bool>,
) {
    let mut pipeline_after_tree = zksync_os_pipeline::Pipeline::new()
        .pipe(ExternalNodeCommandSource {
            starting_block,
            replay_download_address: config
                .sequencer_config
                .block_replay_download_address
                .clone()
                .expect("EN must have replay_download_address"),
        })
        .pipe(Sequencer {
            block_context_provider,
            state: state.clone(),
            wal: block_replay_storage.clone(),
            repositories: repositories.clone(),
            sequencer_config: config.sequencer_config.clone().into(),
        })
        .pipe(TreeManager { tree: tree.clone() })
        .into_receiver();

    // Drain the pipeline output
    tokio::spawn(async move { while pipeline_after_tree.recv().await.is_some() {} });

    // Channel between `PriorityTree` tasks
    let (priority_txs_internal_sender, priority_txs_internal_receiver) =
        tokio::sync::mpsc::channel::<(u64, u64, Option<usize>)>(1000);

    // Input channels for `PriorityTree` tasks
    let (executed_batch_numbers_sender_1, executed_batch_numbers_receiver_1) =
        tokio::sync::mpsc::channel::<u64>(1000);
    let (executed_batch_numbers_sender_2, executed_batch_numbers_receiver_2) =
        tokio::sync::mpsc::channel::<u64>(1000);

    let last_ready_block = node_state_on_startup
        .last_l1_executed_block
        .min(node_state_on_startup.block_replay_storage_last_block);
    let batch_of_last_ready_block = batch_storage
        .get_batch_by_block_number(last_ready_block)
        .await
        .unwrap()
        .unwrap();
    let batch_range = batch_storage
        .get_batch_range_by_number(batch_of_last_ready_block)
        .await
        .unwrap()
        .unwrap();
    let last_ready_batch = if last_ready_block == batch_range.1 {
        batch_of_last_ready_block
    } else {
        batch_of_last_ready_block.saturating_sub(1)
    };
    let init_block = batch_storage
        .get_batch_range_by_number(last_ready_batch)
        .await
        .unwrap()
        .unwrap()
        .1;

    let priority_tree_manager = PriorityTreeManager::new(
        block_replay_storage.clone(),
        init_block,
        Path::new(
            &config
                .general_config
                .rocks_db_path
                .join(PRIORITY_TREE_DB_NAME),
        ),
    )
    .unwrap();
    // Prepare a stream of executed batch numbers for the first task.
    tasks.spawn(
        util::finalized_block_channel::send_executed_and_replayed_batch_numbers(
            executed_batch_numbers_sender_1,
            batch_storage.clone(),
            finality.clone(),
            Some(block_replay_storage.clone()),
            last_ready_batch + 1,
        )
        .map(report_exit("send_executed_batch_numbers#1")),
    );
    // Run task that adds new txs to PriorityTree. Unlike in MN, we do not provide a sender for ExecuteCommand.
    // Also, channel of executed batch numbers serves as an input.
    tasks.spawn(
        priority_tree_manager
            .clone()
            .prepare_execute_commands(
                batch_storage.clone(),
                None,
                Some(executed_batch_numbers_receiver_1),
                priority_txs_internal_sender,
                None,
            )
            .map(report_exit(
                "priority_tree_manager#prepare_execute_commands",
            )),
    );

    // Prepare a stream of executed batch numbers for the second task.
    tasks.spawn(
        util::finalized_block_channel::send_executed_and_replayed_batch_numbers(
            executed_batch_numbers_sender_2,
            batch_storage.clone(),
            finality,
            Some(block_replay_storage),
            last_ready_batch + 1,
        )
        .map(report_exit("send_executed_batch_numbers#2")),
    );
    // Run task that persists PriorityTree for executed batches and drops old data.
    tasks.spawn(
        priority_tree_manager
            .keep_caching(
                executed_batch_numbers_receiver_2,
                priority_txs_internal_receiver,
            )
            .map(report_exit("priority_tree_manager#keep_caching")),
    );
}

fn block_hashes_for_first_block(repositories: &RepositoryManager) -> BlockHashes {
    let mut block_hashes = BlockHashes::default();
    let genesis_block = repositories
        .get_block_by_number(0)
        .expect("Failed to read genesis block from repositories")
        .expect("Missing genesis block in repositories");
    block_hashes.0[255] = U256::from_be_slice(genesis_block.hash().as_slice());
    block_hashes
}

fn report_exit<T, E: std::fmt::Debug>(name: &'static str) -> impl Fn(Result<T, E>) {
    move |result| match result {
        Ok(_) => tracing::warn!("{name} unexpectedly exited"),
        Err(e) => tracing::error!("{name} failed: {e:#?}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_l1_senders(
    l1_sender_config: L1SenderConfig,
    provider: impl Provider + WalletProvider<Wallet = EthereumWallet> + Clone + 'static,

    batch_for_commit_receiver: Receiver<CommitCommand>,
    batch_for_snark_sender: Sender<BatchEnvelope<FriProof>>,

    batch_for_l1_proving_receiver: Receiver<ProofCommand>,
    batch_for_priority_tree_sender: Sender<BatchEnvelope<FriProof>>,

    batch_for_execute_receiver: Receiver<ExecuteCommand>,
    fully_processed_batch_sender: Sender<BatchEnvelope<FriProof>>,

    l1_state: &L1State,
) -> (
    impl Future<Output = Result<()>> + 'static,
    impl Future<Output = Result<()>> + 'static,
    impl Future<Output = Result<()>> + 'static,
) {
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
    let l1_committer = run_l1_sender(
        batch_for_commit_receiver,
        batch_for_snark_sender,
        l1_state.validator_timelock,
        provider.clone(),
        l1_sender_config.clone().into(),
    );

    let l1_proof_submitter = run_l1_sender(
        batch_for_l1_proving_receiver,
        batch_for_priority_tree_sender,
        l1_state.validator_timelock,
        provider.clone(),
        l1_sender_config.clone().into(),
    );

    let l1_executor = run_l1_sender(
        batch_for_execute_receiver,
        fully_processed_batch_sender,
        l1_state.validator_timelock,
        provider,
        l1_sender_config.clone().into(),
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
            .get(batch_to_prove)
            .await?
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
            .get(batch_to_execute)
            .await?
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

async fn commit_proof_execute_block_numbers(
    l1_state: &L1State,
    batch_storage: &ProofStorage,
) -> (u64, u64, u64) {
    let last_committed_block = if l1_state.last_committed_batch == 0 {
        0
    } else {
        batch_storage
            .get(l1_state.last_committed_batch)
            .await
            .expect("Failed to get last committed block from proof storage")
            .map(|envelope| envelope.batch.last_block_number)
            .expect("Committed batch is not present in proof storage")
    };

    // only used to log on node startup
    let last_proved_block = if l1_state.last_proved_batch == 0 {
        0
    } else {
        batch_storage
            .get(l1_state.last_proved_batch)
            .await
            .expect("Failed to get last proved block from proof storage")
            .map(|envelope| envelope.batch.last_block_number)
            .expect("Proved batch is not present in proof storage")
    };

    let last_executed_block = if l1_state.last_executed_batch == 0 {
        0
    } else {
        batch_storage
            .get(l1_state.last_executed_batch)
            .await
            .expect("Failed to get last proved block from execute storage")
            .map(|envelope| envelope.batch.last_block_number)
            .expect("Execute batch is not present in proof storage")
    };
    (last_committed_block, last_proved_block, last_executed_block)
}

fn run_fake_snark_provers(
    config: &ProverApiConfig,
    tasks: &mut JoinSet<()>,
    snark_job_manager: Arc<SnarkJobManager>,
) {
    tracing::info!(
        max_batch_age = ?config.fake_snark_provers.max_batch_age,
        "Initializing fake SNARK prover"
    );
    let fake_provers_pool = FakeSnarkProver::new(
        snark_job_manager.clone(),
        config.fake_snark_provers.max_batch_age,
    );
    tasks.spawn(
        fake_provers_pool
            .run()
            .map(report_exit("fake_snark_provers_task_optional")),
    );
}

fn run_fake_fri_provers(
    config: &ProverApiConfig,
    tasks: &mut JoinSet<()>,
    fri_job_manager: Arc<FriJobManager>,
) {
    tracing::info!(
        workers = config.fake_fri_provers.workers,
        compute_time = ?config.fake_fri_provers.compute_time,
        min_task_age = ?config.fake_fri_provers.min_age,
        "Initializing fake FRI provers"
    );
    let fake_provers_pool = FakeFriProversPool::new(
        fri_job_manager.clone(),
        config.fake_fri_provers.workers,
        config.fake_fri_provers.compute_time,
        config.fake_fri_provers.min_age,
    );
    tasks.spawn(
        fake_provers_pool
            .run()
            .map(report_exit("fake_fri_provers_task_optional")),
    );
}
