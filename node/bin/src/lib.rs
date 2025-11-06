#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]
mod batch_sink;
pub mod batcher;
mod command_source;
pub mod config;
mod en_remote_config;
mod l1_provider;
pub mod metadata;
mod node_state_on_startup;
mod priority_tree_steps;
pub mod prover_api;
mod prover_input_generator;
mod replay_transport;
mod state_initializer;
pub mod tree_manager;
pub mod zkstack_config;

use crate::batch_sink::{BatchSink, NoOpSink};
use crate::batcher::{Batcher, util::load_genesis_stored_batch_info};
use crate::command_source::{ExternalNodeCommandSource, MainNodeCommandSource};
use crate::config::{Config, ProverApiConfig, gas_adjuster_config};
use crate::en_remote_config::load_remote_config;
use crate::l1_provider::build_node_l1_provider;
use crate::metadata::NODE_VERSION;
use crate::node_state_on_startup::NodeStateOnStartup;
use crate::priority_tree_steps::priority_tree_en_step::PriorityTreeENStep;
use crate::priority_tree_steps::priority_tree_pipeline_step::PriorityTreePipelineStep;
use crate::prover_api::fake_fri_provers_pool::FakeFriProversPool;
use crate::prover_api::fri_job_manager::FriJobManager;
use crate::prover_api::fri_proving_pipeline_step::FriProvingPipelineStep;
use crate::prover_api::gapless_committer::GaplessCommitter;
use crate::prover_api::proof_storage::ProofStorage;
use crate::prover_api::prover_server;
use crate::prover_api::snark_job_manager::{FakeSnarkProver, SnarkJobManager};
use crate::prover_api::snark_proving_pipeline_step::SnarkProvingPipelineStep;
use crate::prover_input_generator::ProverInputGenerator;
use crate::replay_transport::replay_server;
use crate::state_initializer::StateInitializer;
use crate::tree_manager::TreeManager;
use alloy::network::EthereumWallet;
use alloy::providers::{Provider, WalletProvider};
use anyhow::{Context, Result};
use futures::FutureExt;
use ruint::aliases::U256;
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::task::JoinSet;
use zksync_os_batch_verification::{BatchVerificationClient, BatchVerificationPipelineStep};
use zksync_os_contract_interface::l1_discovery::L1State;
use zksync_os_gas_adjuster::GasAdjuster;
use zksync_os_genesis::{FileGenesisInputSource, Genesis, GenesisInputSource};
use zksync_os_interface::types::BlockHashes;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_l1_sender::pipeline_component::L1Sender;
use zksync_os_l1_watcher::{L1CommitWatcher, L1ExecuteWatcher, L1TxWatcher};
use zksync_os_mempool::L2TransactionPool;
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_object_store::ObjectStoreFactory;
use zksync_os_observability::GENERAL_METRICS;
use zksync_os_pipeline::Pipeline;
use zksync_os_revm_consistency_checker::node::RevmConsistencyChecker;
use zksync_os_rpc::{RpcStorage, run_jsonrpsee_server};
use zksync_os_sequencer::execution::Sequencer;
use zksync_os_sequencer::execution::block_context_provider::BlockContextProvider;
use zksync_os_status_server::run_status_server;
use zksync_os_storage::db::BlockReplayStorage;
use zksync_os_storage::in_memory::Finality;
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::{
    FinalityStatus, ReadFinality, ReadReplay, ReadRepository, ReadStateHistory, WriteReplay,
    WriteRepository, WriteState,
};
use zksync_os_types::{NotAcceptingReason, TransactionAcceptanceState};

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
    if !config.l1_sender_config.enabled {
        unimplemented!("running without L1 Senders is temporarily not supported");
    }
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
    let fee_collector_address: &'static str = config
        .sequencer_config
        .fee_collector_address
        .to_string()
        .leak();
    GENERAL_METRICS.fee_collector_address[&fee_collector_address].set(1);
    GENERAL_METRICS.chain_id.set(chain_id);

    // Channel between L1Watcher and Sequencer
    let (l1_transactions_sender, l1_transactions_for_sequencer) = tokio::sync::mpsc::channel(5);

    tracing::info!("Initializing BatchStorage");
    let batch_storage = ProofStorage::new(
        ObjectStoreFactory::new(config.prover_api_config.object_store.clone())
            .create_store()
            .await
            .unwrap(),
    );

    // This is the only place where we initialize L1 provider, every component shares the same
    // cloned provider.
    let l1_provider = build_node_l1_provider(&config.general_config.l1_rpc_url).await;

    tracing::info!("Reading L1 state");
    let l1_state = if config.sequencer_config.is_main_node() {
        // On the main node, we need to wait for the pending L1 transactions (commit/prove/execute) to be mined before proceeding.
        L1State::fetch_finalized(l1_provider.clone().erased(), bridgehub_address, chain_id)
            .await
            .expect("failed to fetch finalized L1 state")
    } else {
        L1State::fetch(l1_provider.clone().erased(), bridgehub_address, chain_id)
            .await
            .expect("failed to fetch L1 state")
    };
    tracing::info!(?l1_state, "L1 state");
    l1_state.report_metrics();

    let genesis = Genesis::new(genesis_input_source.clone(), l1_state.diamond_proxy.clone());

    tracing::info!("Initializing BlockReplayStorage");

    let block_replay_storage = BlockReplayStorage::new(
        &config
            .general_config
            .rocks_db_path
            .join(BLOCK_REPLAY_WAL_DB_NAME),
        &genesis,
        node_version.clone(),
    )
    .await;

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
        state.clone(),
        repositories.clone(),
        chain_id,
        config.mempool_config.clone().into(),
        config.tx_validator_config.clone().into(),
    );

    let (last_l1_committed_block, last_l1_proved_block, last_l1_executed_block) =
        commit_proof_execute_block_numbers(&l1_state, &batch_storage).await;

    let node_startup_state = NodeStateOnStartup {
        is_main_node: config.sequencer_config.is_main_node(),
        l1_state: l1_state.clone(),
        state_block_range_available: state.block_range_available(),
        block_replay_storage_last_block: block_replay_storage.latest_record(),
        tree_last_block: tree_db
            .latest_version()
            .expect("cannot read tree last processed block after initialization")
            .expect("tree database is not initialized"),
        repositories_persisted_block: repositories.get_latest_block(),
        last_l1_committed_block,
        last_l1_proved_block,
        last_l1_executed_block,
    };

    if let Some(block_rebuild) = &config.sequencer_config.block_rebuild {
        assert!(
            block_rebuild.from_block > node_startup_state.last_l1_committed_block,
            "rebuild_from_block must be > last_l1_committed_block, got {} <= {}",
            block_rebuild.from_block,
            node_startup_state.last_l1_committed_block
        );
    }

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
        .chain(
            config
                .sequencer_config
                .block_rebuild
                .as_ref()
                .map(|c| c.from_block),
        )
        .min()
        .unwrap()
        .max(1)
    };

    let starting_block = if desired_starting_block < state.block_range_available().start() + 1 {
        // Starting from 1 used to work but with the current compacted state implementation it panics.
        // The approach has to be re-evaluated if we want to keep compacted storage impl and what to do in this case.
        // For now just panic early.
        // TODO(compacted state): re-evaluate the approach.
        panic!(
            "Cannot start: desired_starting_block < state.block_range_available().start() + 1: {} < {}",
            desired_starting_block,
            state.block_range_available().start() + 1
        );

        // tracing::warn!(
        //     desired_starting_block,
        //     config.general_config.force_starting_block_number,
        //     min_block_available_in_state = state.block_range_available().start() + 1,
        //     "Desired starting block is not available in state. Starting from zero."
        // );
        // 1
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
        last_committed_batch: l1_state.last_committed_batch,
        last_executed_block: last_l1_executed_block,
        last_executed_batch: l1_state.last_executed_batch,
    });

    let mut tasks: JoinSet<()> = JoinSet::new();
    tasks.spawn(
        L1CommitWatcher::new(
            config.l1_watcher_config.clone().into(),
            node_startup_state.l1_state.diamond_proxy.clone(),
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
            node_startup_state.l1_state.diamond_proxy.clone(),
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
            node_startup_state.l1_state.diamond_proxy.clone(),
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

    // Transaction acceptance state - tracks whether we're accepting new transactions
    // Main nodes: accepts, but may switch to reject when `sequencer_max_blocks_to_produce` blocks are produced
    // External nodes: always reject
    let (tx_acceptance_state_sender, tx_acceptance_state_receiver) =
        if config.sequencer_config.is_main_node() {
            watch::channel(TransactionAcceptanceState::Accepting)
        } else {
            watch::channel(TransactionAcceptanceState::NotAccepting(
                NotAcceptingReason::ExternalNode,
            ))
        };

    let (pending_block_context_sender, pending_block_context_receiver) = watch::channel(None);
    tasks.spawn(
        run_jsonrpsee_server(
            config.rpc_config.clone().into(),
            chain_id,
            node_startup_state.l1_state.bridgehub_address(),
            rpc_storage,
            l2_mempool.clone(),
            genesis_input_source,
            tx_acceptance_state_receiver,
            pending_block_context_receiver,
        )
        .map(report_exit("JSON-RPC server")),
    );

    tracing::info!("Initializing pubdata price provider");
    let (pubdata_price_sender, pubdata_price_receiver) = watch::channel(None);
    if config.sequencer_config.is_main_node() {
        let gas_adjuster_config = gas_adjuster_config(
            config.gas_adjuster_config.clone(),
            l1_state.da_input_mode,
            config.l1_sender_config.rollup_pubdata_mode,
            config.l1_sender_config.max_priority_fee_per_gas_gwei,
        );
        let gas_adjuster = GasAdjuster::new(
            l1_provider.clone().erased(),
            gas_adjuster_config,
            pubdata_price_sender,
        )
        .await
        .unwrap();
        tasks.spawn(gas_adjuster.run().map(report_exit("Gas adjuster server")));
    }

    // ========== Start BlockContextProvider and its state ===========
    tracing::info!("Initializing BlockContextProvider");

    let previous_block_timestamp: u64 = first_replay_record
        .as_ref()
        .map_or(0, |record| record.previous_block_timestamp); // if no previous block, assume genesis block

    let block_hashes_for_next_block = first_replay_record
        .as_ref()
        .map(|record| record.block_context.block_hashes)
        .unwrap_or_else(|| block_hashes_for_first_block(&repositories));

    let genesis = Arc::new(genesis);
    // todo: `BlockContextProvider` initialization and its dependencies
    // should be moved to `sequencer`
    let block_context_provider = BlockContextProvider::new(
        next_l1_priority_id,
        l1_transactions_for_sequencer,
        l2_mempool,
        block_hashes_for_next_block,
        previous_block_timestamp,
        chain_id,
        config.sequencer_config.block_gas_limit,
        config.sequencer_config.block_pubdata_limit_bytes,
        node_version,
        genesis.clone(),
        config.sequencer_config.fee_collector_address,
        config.sequencer_config.base_fee_override,
        config.sequencer_config.pubdata_price_override,
        config.sequencer_config.native_price_override,
        pubdata_price_receiver,
        pending_block_context_sender,
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
        run_main_node_pipeline(
            config,
            l1_provider.clone(),
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
            &genesis,
            _stop_receiver.clone(),
            tx_acceptance_state_sender,
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
            tx_acceptance_state_sender,
        )
        .await;
    };
    let startup_time = process_started_at.elapsed();
    GENERAL_METRICS.startup_time[&"total"].set(startup_time.as_secs_f64());
    tracing::info!("All components initialized in {startup_time:?}");
    tasks.join_next().await;
    tracing::info!("One of the subsystems exited - exiting process.");
}

#[allow(clippy::too_many_arguments)]
async fn run_main_node_pipeline(
    config: Config,
    l1_provider: impl Provider + WalletProvider<Wallet = EthereumWallet> + Clone + 'static,
    batch_storage: ProofStorage,
    node_state_on_startup: NodeStateOnStartup,
    block_replay_storage: impl WriteReplay + Clone,
    tasks: &mut JoinSet<()>,
    state: impl ReadStateHistory + WriteState + Clone,
    starting_block: u64,
    repositories: impl WriteRepository + Clone,
    block_context_provider: BlockContextProvider<impl L2TransactionPool>,
    tree: MerkleTree<RocksDBWrapper>,
    finality: impl ReadFinality + Clone,
    chain_id: u64,
    genesis: &Genesis,
    _stop_receiver: watch::Receiver<bool>,
    tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
) {
    let last_committed_batch_info = if node_state_on_startup.l1_state.last_committed_batch == 0 {
        let genesis_block = repositories
            .get_block_by_number(0)
            .expect("Failed to read genesis block from repositories")
            .expect("Missing genesis block in repositories");
        load_genesis_stored_batch_info(
            genesis_block,
            tree.clone(),
            genesis.state().await.expected_genesis_root,
        )
        .await
        .unwrap()
    } else {
        batch_storage
            .get_batch_with_proof(node_state_on_startup.l1_state.last_committed_batch)
            .await
            .expect("Failed to get last committed block from proof storage")
            .expect("Committed batch is not present in proof storage")
            .batch
            .batch_info
            .into_stored()
    };

    // There may be batches that are Committed but not Proven on L1 yet, or Proven but not Executed yet.
    // We will reschedule them by loading `BatchEnvelope`s from ProofStorage
    // and sending them to the corresponding channels.
    let committed_not_proven_batches =
        get_committed_not_proven_batches(&node_state_on_startup.l1_state, &batch_storage)
            .await
            .expect("Cannot get committed not proven batches");

    let proven_not_executed_batches =
        get_proven_not_executed_batches(&node_state_on_startup.l1_state, &batch_storage)
            .await
            .expect("Cannot get proven not executed batches");

    let batcher_subsystem_first_block_to_process =
        node_state_on_startup.last_l1_committed_block + 1;

    let prover_input_generation_first_block_to_process = if config
        .prover_input_generator_config
        .force_process_old_blocks
    {
        // Prover input generator will (re)process all the blocks replayed by the node on startup - at least `min_blocks_to_replay`.
        // It will process all the blocks sent to it from upstream - not necessarily start from `0`.
        0
    } else {
        // Prover input generator will skip the blocks that are already FRI proved and committed to L1.
        batcher_subsystem_first_block_to_process
    };

    let (fri_proving_step, fri_job_manager) = FriProvingPipelineStep::new(
        batch_storage.clone(),
        config.prover_api_config.job_timeout,
        config.prover_api_config.max_assigned_batch_range,
    );

    let (snark_proving_step, snark_job_manager) =
        SnarkProvingPipelineStep::new(config.prover_api_config.max_fris_per_snark);

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

    let priority_tree_db_path = config
        .general_config
        .rocks_db_path
        .join(PRIORITY_TREE_DB_NAME);

    Pipeline::new()
        .pipe(MainNodeCommandSource {
            block_replay_storage: block_replay_storage.clone(),
            starting_block,
            block_time: config.sequencer_config.block_time,
            max_transactions_in_block: config.sequencer_config.max_transactions_in_block,
            rebuild_options: config
                .sequencer_config
                .block_rebuild
                .clone()
                .map(Into::into),
        })
        .pipe(Sequencer {
            block_context_provider,
            state: state.clone(),
            replay: block_replay_storage.clone(),
            repositories: repositories.clone(),
            sequencer_config: config.sequencer_config.clone().into(),
            tx_acceptance_state_sender,
        })
        .pipe_opt(
            config
                .sequencer_config
                .revm_consistency_checker_enabled
                .then(|| RevmConsistencyChecker::new(state.clone())),
        )
        .pipe(TreeManager { tree: tree.clone() })
        .pipe(ProverInputGenerator {
            enable_logging: config.prover_input_generator_config.logging_enabled,
            maximum_in_flight_blocks: config
                .prover_input_generator_config
                .maximum_in_flight_blocks,
            first_block_to_process: prover_input_generation_first_block_to_process,
            app_bin_base_path: config.general_config.rocks_db_path.join("app_bins").clone(),
            read_state: state.clone(),
        })
        .pipe(Batcher {
            chain_id,
            chain_address: node_state_on_startup.l1_state.diamond_proxy_address(),
            first_block_to_process: batcher_subsystem_first_block_to_process,
            last_persisted_block: node_state_on_startup.repositories_persisted_block,
            pubdata_limit_bytes: config.sequencer_config.block_pubdata_limit_bytes,
            batcher_config: config.batcher_config.clone(),
            prev_batch_info: last_committed_batch_info,
        })
        .pipe(BatchVerificationPipelineStep::new(
            config.batch_verification_config.into(),
        ))
        .pipe(fri_proving_step)
        .pipe(GaplessCommitter {
            next_expected: node_state_on_startup.l1_state.last_committed_batch + 1,
            proof_storage: batch_storage.clone(),
            da_input_mode: node_state_on_startup.l1_state.da_input_mode,
        })
        .pipe(L1Sender::<_, CommitCommand> {
            provider: l1_provider.clone(),
            config: config.l1_sender_config.clone().into(),
            to_address: node_state_on_startup.l1_state.validator_timelock,
        })
        .pipe_with_prepend(snark_proving_step, committed_not_proven_batches)
        .pipe(L1Sender::<_, ProofCommand> {
            provider: l1_provider.clone(),
            config: config.l1_sender_config.clone().into(),
            to_address: node_state_on_startup.l1_state.validator_timelock,
        })
        .pipe_with_prepend(
            PriorityTreePipelineStep::new(
                block_replay_storage.clone(),
                &priority_tree_db_path,
                batch_storage.clone(),
                finality,
            )
            .await
            .unwrap(),
            proven_not_executed_batches,
        )
        .pipe(L1Sender {
            provider: l1_provider,
            config: config.l1_sender_config.clone().into(),
            to_address: node_state_on_startup.l1_state.validator_timelock,
        })
        .pipe(BatchSink)
        .spawn(tasks);
}

/// Only for EN - we still populate channels destined for the batcher subsystem -
/// need to drain them to not get stuck
#[allow(clippy::too_many_arguments)]
async fn run_en_pipeline(
    config: Config,
    batch_storage: ProofStorage,
    node_state_on_startup: NodeStateOnStartup,
    block_replay_storage: impl WriteReplay + Clone,
    tasks: &mut JoinSet<()>,
    block_context_provider: BlockContextProvider<impl L2TransactionPool>,
    state: impl ReadStateHistory + WriteState + Clone,
    tree: MerkleTree<RocksDBWrapper>,
    starting_block: u64,
    repositories: impl WriteRepository + Clone,
    finality: impl ReadFinality + Clone,
    _stop_receiver: watch::Receiver<bool>,
    tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
) {
    Pipeline::new()
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
            replay: block_replay_storage.clone(),
            repositories: repositories.clone(),
            sequencer_config: config.sequencer_config.clone().into(),
            tx_acceptance_state_sender,
        })
        .pipe_opt(
            config
                .sequencer_config
                .revm_consistency_checker_enabled
                .then(|| RevmConsistencyChecker::new(state.clone())),
        )
        .pipe(TreeManager { tree: tree.clone() })
        .pipe_if(
            config.batch_verification_config.client_enabled,
            BatchVerificationClient::new(
                finality.clone(),
                config.batch_verification_config.signing_key.clone(),
                config.genesis_config.chain_id.unwrap(),
                *node_state_on_startup.l1_state.diamond_proxy.address(),
                config.batch_verification_config.connect_address,
            ),
            NoOpSink::new(),
        )
        .spawn(tasks);

    // Run Priority Tree tasks for EN - not part of the pipeline.
    let priority_tree_en_step = PriorityTreeENStep::new(
        block_replay_storage,
        Path::new(
            &config
                .general_config
                .rocks_db_path
                .join(PRIORITY_TREE_DB_NAME),
        ),
        batch_storage,
        finality,
        node_state_on_startup
            .last_l1_executed_block
            .min(node_state_on_startup.block_replay_storage_last_block),
    )
    .await
    .unwrap();

    tasks.spawn(
        priority_tree_en_step
            .run()
            .map(report_exit("priority_tree_en")),
    );
}

fn block_hashes_for_first_block(repositories: &dyn ReadRepository) -> BlockHashes {
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
        Ok(_) => tracing::warn!("{name} component unexpectedly exited"),
        Err(err) => tracing::error!(?err, "{name} component failed"),
    }
}

async fn get_committed_not_proven_batches(
    l1_state: &L1State,
    proof_storage: &ProofStorage,
) -> anyhow::Result<Vec<SignedBatchEnvelope<FriProof>>> {
    let mut batch_to_prove = l1_state.last_proved_batch + 1;
    let mut batches_to_reschedule = Vec::new();
    while batch_to_prove <= l1_state.last_committed_batch {
        let batch_with_proof = proof_storage
            .get_batch_with_proof(batch_to_prove)
            .await?
            .context("Failed to get batch")?;
        batches_to_reschedule.push(batch_with_proof);
        batch_to_prove += 1;
    }
    Ok(batches_to_reschedule)
}

async fn get_proven_not_executed_batches(
    l1_state: &L1State,
    proof_storage: &ProofStorage,
) -> Result<Vec<SignedBatchEnvelope<FriProof>>> {
    let mut batch_to_execute = l1_state.last_executed_batch + 1;
    let mut batches_to_reschedule = Vec::new();
    while batch_to_execute <= l1_state.last_proved_batch {
        let batch_with_proof = proof_storage
            .get_batch_with_proof(batch_to_execute)
            .await?
            .context("Failed to get batch")?;
        batches_to_reschedule.push(batch_with_proof);
        batch_to_execute += 1;
    }
    Ok(batches_to_reschedule)
}

async fn commit_proof_execute_block_numbers(
    l1_state: &L1State,
    batch_storage: &ProofStorage,
) -> (u64, u64, u64) {
    let last_committed_block = if l1_state.last_committed_batch == 0 {
        0
    } else {
        batch_storage
            .get_batch_with_proof(l1_state.last_committed_batch)
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
            .get_batch_with_proof(l1_state.last_proved_batch)
            .await
            .expect("Failed to get last proved block from proof storage")
            .map(|envelope| envelope.batch.last_block_number)
            .expect("Proved batch is not present in proof storage")
    };

    let last_executed_block = if l1_state.last_executed_batch == 0 {
        0
    } else {
        batch_storage
            .get_batch_with_proof(l1_state.last_executed_batch)
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
