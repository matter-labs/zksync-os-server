#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]
mod batch_sink;
pub mod batcher;
pub mod block_replay_storage;
pub mod config;
mod metadata;
mod metrics;
mod node_state_on_startup;
pub mod prover_api;
mod prover_input_generator;
mod replay_transport;
pub mod reth_state;
mod state_initializer;
pub mod tree_manager;
mod util;
pub mod zkstack_config;

use crate::batch_sink::BatchSink;
use crate::batcher::{Batcher, util::load_genesis_stored_batch_info};
use crate::block_replay_storage::BlockReplayStorage;
use crate::config::{Config, ProverApiConfig};
use crate::metadata::NODE_VERSION;
use crate::metrics::NODE_META_METRICS;
use crate::node_state_on_startup::NodeStateOnStartup;
use crate::prover_api::fake_fri_provers_pool::FakeFriProversPool;
use crate::prover_api::fri_job_manager::FriJobManager;
use crate::prover_api::gapless_committer::GaplessCommitter;
use crate::prover_api::proof_storage::ProofStorage;
use crate::prover_api::prover_server;
use crate::prover_api::snark_job_manager::{FakeSnarkProver, SnarkJobManager};
use crate::prover_input_generator::ProverInputGenerator;
use crate::replay_transport::{replay_receiver, replay_server};
use crate::reth_state::ZkClient;
use crate::state_initializer::StateInitializer;
use crate::tree_manager::TreeManager;
use crate::util::peekable_receiver::PeekableReceiver;
use alloy::network::EthereumWallet;
use alloy::providers::{Provider, ProviderBuilder, WalletProvider};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::BoxStream;
use ruint::aliases::U256;
use smart_config::value::ExposeSecret;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::task::JoinSet;
use zksync_os_genesis::Genesis;
use zksync_os_interface::common_types::{BlockHashes, BlockOutput};
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof, ProverInput};
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_l1_sender::l1_discovery::{L1State, get_l1_state};
use zksync_os_l1_sender::run_l1_sender;
use zksync_os_l1_watcher::{L1CommitWatcher, L1ExecuteWatcher, L1TxWatcher};
use zksync_os_merkle_tree::{MerkleTreeForReading, RocksDBWrapper};
use zksync_os_multivm::ZKsyncOSVersion;
use zksync_os_object_store::ObjectStoreFactory;
use zksync_os_priority_tree::PriorityTreeManager;
use zksync_os_rpc::{RpcStorage, run_jsonrpsee_server};
use zksync_os_sequencer::execution::block_context_provider::BlockContextProvider;
use zksync_os_sequencer::execution::run_sequencer_actor;
use zksync_os_sequencer::model::blocks::{BlockCommand, ProduceCommand};
use zksync_os_storage::in_memory::Finality;
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::{
    FinalityStatus, ReadReplay, ReadRepository, ReadStateHistory, ReplayRecord, RepositoryBlock,
    WriteState,
};

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const TREE_DB_NAME: &str = "tree";
const REPOSITORY_DB_NAME: &str = "repository";

#[allow(clippy::too_many_arguments)]
pub async fn run<State: ReadStateHistory + WriteState + StateInitializer + Clone>(
    _stop_receiver: watch::Receiver<bool>,
    config: Config,
) {
    let node_version: semver::Version = NODE_VERSION.parse().unwrap();
    NODE_META_METRICS.version[&NODE_VERSION].set(1);
    let role: &'static str = if config.sequencer_config.is_main_node() {
        "main_node"
    } else {
        "external_node"
    };
    NODE_META_METRICS.role[&role].set(1);

    tracing::info!(version = %node_version, "Initializing Node");

    let (blocks_for_batcher_subsystem_sender, blocks_for_batcher_subsystem_receiver) =
        tokio::sync::mpsc::channel::<(BlockOutput, ReplayRecord)>(5);

    // Channel between L1Watcher and Sequencer
    let (l1_transactions_sender, l1_transactions) = tokio::sync::mpsc::channel(5);

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
        config.genesis_config.chain_id,
        node_version.clone(),
        ZKsyncOSVersion::latest().into(),
    );

    // This is the only place where we initialize L1 provider, every component shares the same
    // cloned provider.
    let l1_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(PrivateKeySigner::random()))
        .connect(&config.general_config.l1_rpc_url)
        .await
        .expect("failed to connect to L1 api");

    tracing::info!("Reading L1 state");
    let l1_state = get_l1_state(
        &l1_provider,
        config.genesis_config.bridgehub_address,
        config.genesis_config.chain_id,
    )
    .await
    .expect("Failed to read L1 state");
    tracing::info!(?l1_state, "L1 state");

    tracing::info!("Initializing TreeManager");
    let tree_wrapper = TreeManager::tree_wrapper(Path::new(
        &config.general_config.rocks_db_path.join(TREE_DB_NAME),
    ));

    let genesis = Genesis::new(
        config.genesis_config.genesis_input_path.clone(),
        l1_provider.clone().erased(),
        l1_state.diamond_proxy,
    );

    // Channel between `Sequencer` and `Tree` - note that sequencer doesn't need tree to continue so it can be async.
    // Only Batcher Subsystem depends on the tree, but we run it on ENs as well for quick failover.
    let (tree_sender, tree_receiver) = tokio::sync::mpsc::channel::<BlockOutput>(5);

    tracing::info!("Initializing RepositoryManager");
    let repositories = RepositoryManager::new(
        config.general_config.blocks_to_retain_in_memory,
        config.general_config.rocks_db_path.join(REPOSITORY_DB_NAME),
        &genesis,
    );

    let state = State::new(&config.general_config, &genesis).await;

    tracing::info!("Initializing mempools");
    let l2_mempool = zksync_os_mempool::in_memory(
        ZkClient::new(
            repositories.clone(),
            state.clone(),
            config.genesis_config.chain_id,
        ),
        config.mempool_config.max_tx_input_bytes,
    );

    let (tree_manager, persistent_tree) =
        TreeManager::new(tree_wrapper.clone(), tree_receiver, &genesis);

    let (last_committed_block, last_proved_block, last_executed_block) =
        commit_proof_execute_block_numbers(&l1_state, &batch_storage).await;

    let node_startup_state = NodeStateOnStartup {
        is_main_node: config.sequencer_config.is_main_node(),
        l1_state,
        state_block_range_available: state.block_range_available(),
        block_replay_storage_last_block: block_replay_storage.latest_block().unwrap_or(0),
        tree_last_block: tree_manager
            .last_processed_block()
            .expect("cannot read tree last processed block after initialization"),
        repositories_persisted_block: repositories.get_latest_block(),
        last_committed_block,
        last_proved_block,
        last_executed_block,
    };

    let desired_starting_block = [
        node_startup_state
            .block_replay_storage_last_block
            .saturating_sub(config.general_config.min_blocks_to_replay as u64),
        node_startup_state.last_committed_block + 1,
        node_startup_state.repositories_persisted_block + 1,
        node_startup_state.tree_last_block + 1,
        state.block_range_available().end() + 1,
    ]
    .into_iter()
    .min()
    .unwrap();

    let starting_block = if desired_starting_block < state.block_range_available().start() + 1 {
        tracing::warn!(
            desired_starting_block,
            min_block_available_in_state = state.block_range_available().start() + 1,
            "Desired starting block is not available in state. Starting from zero."
        );
        1
    } else {
        desired_starting_block
    };

    tracing::info!(
        config.general_config.min_blocks_to_replay,
        ?node_startup_state,
        starting_block,
        "Node state on startup"
    );

    node_startup_state.assert_consistency();

    tracing::info!("Initializing L1 Watchers");
    let finality_storage = Finality::new(FinalityStatus {
        last_committed_block,
        last_executed_block,
    });

    let mut tasks: JoinSet<()> = JoinSet::new();
    tasks.spawn(tree_manager.run_loop().map(report_exit("TREE server")));
    tasks.spawn(
        L1CommitWatcher::new(
            config.l1_watcher_config.clone(),
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
            config.l1_watcher_config.clone(),
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
            config.l1_watcher_config.clone(),
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

    // =========== Start JSON RPC ========

    let rpc_storage = RpcStorage::new(
        repositories.clone(),
        block_replay_storage.clone(),
        finality_storage,
        batch_storage.clone(),
        state.clone(),
    );

    tasks.spawn(
        run_jsonrpsee_server(
            config.rpc_config.clone(),
            config.genesis_config.chain_id,
            node_startup_state.l1_state.bridgehub,
            rpc_storage,
            l2_mempool.clone(),
        )
        .map(report_exit("JSON-RPC server")),
    );

    // ========== Start BlockContextProvider and its state ===========
    tracing::info!("Initializing BlockContextProvider");

    let previous_block_timestamp: u64 = first_replay_record
        .as_ref()
        .map_or(0, |record| record.block_context.timestamp); // if no previous block, assume genesis block

    let block_hashes_for_next_block = first_replay_record
        .as_ref()
        .map(|record| record.block_context.block_hashes)
        .unwrap_or_else(|| block_hashes_for_first_block(&repositories));

    let command_block_context_provider = BlockContextProvider::new(
        next_l1_priority_id,
        l1_transactions,
        l2_mempool,
        block_hashes_for_next_block,
        previous_block_timestamp,
        config.genesis_config.chain_id,
        config.sequencer_config.block_gas_limit,
        config.sequencer_config.block_pubdata_limit_bytes,
        node_version,
        genesis,
    );

    // ========== Start Sequencer ===========
    tasks.spawn(
        replay_server(
            block_replay_storage.clone(),
            config.sequencer_config.block_replay_server_address.clone(),
        )
        .map(report_exit("replay server")),
    );
    {
        let state = state.clone();
        let block_replay_storage = block_replay_storage.clone();
        let repositories = repositories.clone();
        let sequencer_config = config.sequencer_config.clone();

        tasks.spawn(async move {
            let block_stream = if let Some(replay_download_address) =
                &sequencer_config.block_replay_download_address
            {
                // External Node
                match replay_receiver(starting_block, replay_download_address).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        tracing::error!("Failed to connect to main node to receive blocks: {e}");
                        return;
                    }
                }
            } else {
                // Main Node
                command_source(
                    &block_replay_storage,
                    starting_block,
                    sequencer_config.block_time,
                    sequencer_config.max_transactions_in_block,
                )
            };

            run_sequencer_actor(
                block_stream,
                blocks_for_batcher_subsystem_sender,
                tree_sender,
                command_block_context_provider,
                state,
                block_replay_storage.clone(),
                repositories,
                sequencer_config,
            )
            .map(report_exit("Sequencer server"))
            .await
        });
    }
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
        run_batcher_subsystem(
            config,
            l1_provider.clone(),
            genesis_block,
            persistent_tree,
            batch_storage,
            node_startup_state,
            blocks_for_batcher_subsystem_receiver,
            block_replay_storage,
            &mut tasks,
            state,
            _stop_receiver.clone(),
        )
        .await;
    } else {
        // External Node
        run_batcher_channel_drain(
            _stop_receiver.clone(),
            blocks_for_batcher_subsystem_receiver,
        )
        .await;
    };
    tasks.join_next().await;
}

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

/// Only for EN - we still populate channels destined for the batcher subsystem -
/// need to drain them to not get stuck
async fn run_batcher_channel_drain(
    _stop_receiver: watch::Receiver<bool>,
    mut blocks_for_batcher_subsystem_receiver: Receiver<(BlockOutput, ReplayRecord)>,
) {
    //todo
    tokio::spawn(
        async move { while blocks_for_batcher_subsystem_receiver.recv().await.is_some() {} },
    );
}

/// Only for Main Node
#[allow(clippy::too_many_arguments)]
async fn run_batcher_subsystem<State: ReadStateHistory + Clone>(
    config: Config,
    l1_provider: impl Provider + WalletProvider<Wallet = EthereumWallet> + Clone + 'static,
    genesis_block: RepositoryBlock,
    persistent_tree: MerkleTreeForReading<RocksDBWrapper>,
    batch_storage: ProofStorage,
    node_state_on_startup: NodeStateOnStartup,
    blocks_for_batcher_subsystem_receiver: Receiver<(BlockOutput, ReplayRecord)>,
    block_replay_storage: BlockReplayStorage,
    tasks: &mut JoinSet<()>,
    state: State,
    _stop_receiver: watch::Receiver<bool>,
) {
    // Batcher-specific async channels

    // Channel between `ProverInputGenerator` and `Batcher`
    let (blocks_for_batcher_sender, blocks_for_batcher_receiver) =
        tokio::sync::mpsc::channel::<(BlockOutput, ReplayRecord, ProverInput)>(10);

    // Channel between `Batcher` and `ProverAPI`
    let (batch_for_proving_sender, batch_for_prover_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<ProverInput>>(5);

    // Channel between between `ProverAPI` and `GaplessCommitter`
    let (batch_with_proof_sender, batch_with_proof_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<FriProof>>(5);

    // Channel between `GaplessCommitter` and `L1Committer`
    let (batch_for_commit_sender, batch_for_commit_receiver) =
        tokio::sync::mpsc::channel::<CommitCommand>(5);

    // Channel between `SnarkJobManager` and `L1ProofSubmitter`
    let (batch_for_l1_proving_sender, batch_for_l1_proving_receiver) =
        tokio::sync::mpsc::channel::<ProofCommand>(5);

    // Channel between `PriorityTree` and `L1Executor`
    let (batch_for_execute_sender, batch_for_execute_receiver) =
        tokio::sync::mpsc::channel::<ExecuteCommand>(5);

    // Channel between `L1Executor` and `BatchSink`
    let (fully_processed_batch_sender, fully_processed_batch_receiver) =
        tokio::sync::mpsc::channel::<BatchEnvelope<FriProof>>(5);

    let last_committed_batch_info = if node_state_on_startup.l1_state.last_committed_batch == 0 {
        load_genesis_stored_batch_info(genesis_block, persistent_tree.clone()).await
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

    let batcher_subsystem_first_block_to_process = node_state_on_startup.last_committed_block + 1;
    tracing::info!("Initializing Batcher");
    let batcher = Batcher::new(
        config.genesis_config.chain_id,
        node_state_on_startup.l1_state.diamond_proxy,
        batcher_subsystem_first_block_to_process,
        node_state_on_startup.repositories_persisted_block,
        config.sequencer_config.block_pubdata_limit_bytes,
        config.batcher_config,
        PeekableReceiver::new(blocks_for_batcher_receiver),
        batch_for_proving_sender,
        persistent_tree.clone(),
    );
    tasks.spawn(
        batcher
            .run_loop(last_committed_batch_info)
            .map(report_exit("Batcher")),
    );

    tracing::info!("Initializing ProverInputGenerator");
    let prover_input_generator = ProverInputGenerator::new(
        config.prover_input_generator_config.logging_enabled,
        config
            .prover_input_generator_config
            .maximum_in_flight_blocks,
        batcher_subsystem_first_block_to_process,
        blocks_for_batcher_subsystem_receiver,
        blocks_for_batcher_sender,
        persistent_tree,
        state.clone(),
    );
    tasks.spawn(
        prover_input_generator
            .run_loop()
            .map(report_exit("ProverInputGenerator")),
    );

    let fri_job_manager = Arc::new(FriJobManager::new(
        batch_for_prover_receiver,
        batch_with_proof_sender,
        config.prover_api_config.job_timeout,
        config.prover_api_config.max_assigned_batch_range,
    ));

    let snark_job_manager = Arc::new(SnarkJobManager::new(
        PeekableReceiver::new(batch_for_snark_receiver),
        batch_for_l1_proving_sender,
        config.prover_api_config.max_fris_per_snark,
    ));

    let prover_gapless_committer = GaplessCommitter::new(
        node_state_on_startup.l1_state.last_committed_batch + 1,
        batch_with_proof_receiver,
        batch_storage.clone(),
        batch_for_commit_sender,
        node_state_on_startup.l1_state.da_input_mode,
    );

    tasks.spawn(
        prover_gapless_committer
            .run()
            .map(report_exit("prover_gapless_committer")),
    );

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

    tasks.spawn(
        PriorityTreeManager::new(
            block_replay_storage.clone(),
            node_state_on_startup.last_executed_block,
            batch_for_priority_tree_receiver,
            batch_for_execute_sender,
        )
        .unwrap()
        .run()
        .map(report_exit("Priority tree manager")),
    );

    tasks.spawn(
        BatchSink::new(fully_processed_batch_receiver)
            .run()
            .map(report_exit("batch_sink")),
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

fn report_exit<T, E: std::fmt::Display>(name: &'static str) -> impl Fn(Result<T, E>) {
    move |result| match result {
        Ok(_) => tracing::warn!("{name} unexpectedly exited"),
        Err(e) => tracing::error!("{name} failed: {e:#}"),
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
        l1_sender_config.operator_commit_pk.clone(),
        provider.clone(),
        l1_sender_config.max_fee_per_gas(),
        l1_sender_config.max_priority_fee_per_gas(),
        l1_sender_config.command_limit,
        l1_sender_config.poll_interval,
    );

    let l1_proof_submitter = run_l1_sender(
        batch_for_l1_proving_receiver,
        batch_for_priority_tree_sender,
        l1_state.diamond_proxy,
        l1_sender_config.operator_prove_pk.clone(),
        provider.clone(),
        l1_sender_config.max_fee_per_gas(),
        l1_sender_config.max_priority_fee_per_gas(),
        l1_sender_config.command_limit,
        l1_sender_config.poll_interval,
    );

    let l1_executor = run_l1_sender(
        batch_for_execute_receiver,
        fully_processed_batch_sender,
        l1_state.diamond_proxy,
        l1_sender_config.operator_execute_pk.clone(),
        provider,
        l1_sender_config.max_fee_per_gas(),
        l1_sender_config.max_priority_fee_per_gas(),
        l1_sender_config.command_limit,
        l1_sender_config.poll_interval,
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
