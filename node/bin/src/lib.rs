#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]
mod batch_sink;
pub mod batcher;
mod command_source;
pub mod config;
pub mod default_protocol_version;
mod en_remote_config;
mod node_state_on_startup;
mod priority_tree_steps;
pub mod prover_api;
mod prover_input_generator;
mod provider;
mod state_initializer;
pub mod tree_manager;
pub mod util;

use crate::batch_sink::{BatchSink, NoOpSink, clear_failing_block_config_task};
use crate::batcher::{Batcher, BatcherStartupConfig, util::load_genesis_stored_batch_info};
use crate::command_source::{ConsensusNodeCommandSource, ExternalNodeCommandSource};
use crate::config::{
    Config, ProverApiConfig, base_token_price_updater_config, gas_adjuster_config,
};
use crate::en_remote_config::load_remote_config;
use crate::node_state_on_startup::NodeStateOnStartup;
use crate::priority_tree_steps::priority_tree_en_step::PriorityTreeENStep;
use crate::priority_tree_steps::priority_tree_pipeline_step::PriorityTreePipelineStep;
use crate::prover_api::fake_fri_provers_pool::FakeFriProversPool;
use crate::prover_api::fri_job_manager::FriJobManager;
use crate::prover_api::fri_proving_pipeline_step::FriProvingPipelineStep;
use crate::prover_api::gapless_committer::GaplessCommitter;
use crate::prover_api::gapless_l1_proof_sender::GaplessL1ProofSender;
use crate::prover_api::proof_storage::ProofStorage;
use crate::prover_api::prover_server;
use crate::prover_api::snark_job_manager::{FakeSnarkProver, SnarkJobManager};
use crate::prover_api::snark_proving_pipeline_step::SnarkProvingPipelineStep;
use crate::prover_input_generator::ProverInputGenerator;
use crate::provider::build_node_provider;
use crate::state_initializer::StateInitializer;
use crate::tree_manager::TreeManager;
use alloy::consensus::BlobTransactionSidecar;
use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::BlockNumber;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{Provider, ProviderBuilder, WalletProvider};
use anyhow::{Context, Result};
use futures::FutureExt;
use jsonrpsee::http_client::HttpClient;
use ruint::aliases::U256;
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::task::JoinSet;
use zksync_os_base_token_adjuster::BaseTokenPriceUpdater;
use zksync_os_batch_verification::{BatchVerificationClient, BatchVerificationPipelineStep};
use zksync_os_contract_interface::l1_discovery::{BatchVerificationSL, L1State};
use zksync_os_contract_interface::models::BatchDaInputMode;
use zksync_os_gas_adjuster::GasAdjuster;
use zksync_os_genesis::{FileGenesisInputSource, Genesis, GenesisInputSource};
use zksync_os_interface::types::BlockHashes;
use zksync_os_internal_config::InternalConfigManager;
use zksync_os_interop_fee_updater::{InteropFeeUpdater, InteropFeeUpdaterConfig};
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_l1_sender::pipeline_component::L1Sender;
use zksync_os_l1_sender::upgrade_gatekeeper::UpgradeGatekeeper;
use zksync_os_l1_watcher::{
    CommittedBatchProvider, GatewayMigrationWatcher, L1CommitWatcher, L1ExecuteWatcher,
    L1TxWatcher, L1UpgradeTxWatcher,
};
use zksync_os_l1_watcher::{InteropWatcher, L1PersistBatchWatcher};
use zksync_os_mempool::Pool;
use zksync_os_mempool::subpools::interop_fee::InteropFeeSubpool;
use zksync_os_mempool::subpools::interop_roots::InteropRootsSubpool;
use zksync_os_mempool::subpools::l1::L1Subpool;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_mempool::subpools::sl_chain_id::SlChainIdSubpool;
use zksync_os_mempool::subpools::upgrade::UpgradeSubpool;
use zksync_os_merkle_tree::{MerkleTree, MerkleTreeVersion, RocksDBWrapper};
use zksync_os_metadata::NODE_VERSION;
use zksync_os_network::RecordOverride;
use zksync_os_network::service::{NetworkService, ZksProtocolConfig};
use zksync_os_observability::GENERAL_METRICS;
use zksync_os_pipeline::Pipeline;
use zksync_os_raft::{
    BlockCanonizationEngine, ConsensusRuntimeParts, LeadershipSignal, loopback_consensus,
};
use zksync_os_reth_compat::provider::ZkProviderFactory;
use zksync_os_revm_consistency_checker::node::RevmConsistencyChecker;
use zksync_os_rpc::{EthCallHandler, RpcStorage, run_jsonrpsee_server};
use zksync_os_rpc_api::eth::EthApiClient;
use zksync_os_sequencer::execution::block_context_provider::BlockContextProvider;
use zksync_os_sequencer::execution::{
    BlockApplier, BlockCanonizer, BlockExecutor, FeeParams, FeeProvider,
};
use zksync_os_status_server::run_status_server;
use zksync_os_storage::db::{BlockReplayStorage, ExecutedBatchStorage};
use zksync_os_storage::in_memory::Finality;
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::{
    FinalityStatus, ReadFinality, ReadReplay, ReadRepository, ReadStateHistory, ReplayRecord,
    WriteReplay, WriteRepository, WriteState,
};
use zksync_os_types::{
    ExecutionVersion, InteropRootsLogIndex, ProtocolSemanticVersion, PubdataMode,
    TransactionAcceptanceState, UpgradeInfo, UpgradeMetadata,
};

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const STATE_TREE_DB_NAME: &str = "tree";
const PRIORITY_TREE_DB_NAME: &str = "priority_txs_tree";
const REPOSITORY_DB_NAME: &str = "repository";
const BATCH_DB_NAME: &str = "batch";
pub const INTERNAL_CONFIG_FILE_NAME: &str = "internal_config.json";

#[allow(clippy::too_many_arguments)]
pub async fn run<State: ReadStateHistory + WriteState + StateInitializer + Clone>(
    stop_receiver: watch::Receiver<bool>,
    config: Config,
) {
    let mut tasks: JoinSet<()> = JoinSet::new();

    let node_role = config.general_config.node_role;
    let role: &'static str = node_role.as_str();

    // Priority tree is required for main node
    if node_role.is_main() && !config.general_config.run_priority_tree {
        panic!("`general_run_priority_tree` must be true for Main Node");
    }

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
    tracing::info!(version = NODE_VERSION, role, "Initializing Node");

    let (bridgehub_address, bytecode_supplier_address, chain_id, genesis_input_source) =
        if node_role.is_main() {
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
                config
                    .genesis_config
                    .bytecode_supplier_address
                    .expect("Missing `bytecode_supplier_address`"),
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
                .expect("Cannot load remote config from Main Node")
        };
    let fee_collector_address: &'static str = config
        .sequencer_config
        .fee_collector_address
        .to_string()
        .leak();
    GENERAL_METRICS.fee_collector_address[&fee_collector_address].set(1);
    GENERAL_METRICS.chain_id.set(chain_id);

    // This is the only place where we initialize L1 provider, every component shares the same
    // cloned provider.
    let l1_provider = build_node_provider(&config.general_config.l1_rpc_url).await;
    let sl_provider = match &config.general_config.gateway_rpc_url {
        Some(url) => build_node_provider(url).await,
        None => l1_provider.clone(),
    };
    let gateway_provider = config
        .general_config
        .gateway_rpc_url
        .as_ref()
        .map(|_| sl_provider.clone());

    tracing::info!("Reading L1 state");
    let l1_state = if node_role.is_main() {
        // On the main node, we need to wait for the pending L1 transactions (commit/prove/execute) to be mined before proceeding.
        L1State::fetch_finalized(
            l1_provider.clone().erased(),
            sl_provider.clone().erased(),
            bridgehub_address,
            chain_id,
        )
        .await
        .expect("failed to fetch finalized L1 state")
    } else {
        L1State::fetch(
            l1_provider.clone().erased(),
            sl_provider.clone().erased(),
            bridgehub_address,
            chain_id,
        )
        .await
        .expect("failed to fetch L1 state")
    };
    tracing::info!(?l1_state, "L1 state");
    l1_state.report_metrics();
    if node_role.is_main() {
        check_batch_verification_mismatch(
            &config.batch_verification_config,
            &l1_state.batch_verification,
        );
    }

    if node_role.is_main() {
        let pubdata_mode = config
            .l1_sender_config
            .pubdata_mode
            .expect("l1_sender_pubdata_mode must be set on the Main Node");
        match (pubdata_mode, l1_state.da_input_mode) {
            (
                PubdataMode::Calldata | PubdataMode::Blobs | PubdataMode::RelayedL2Calldata,
                BatchDaInputMode::Validium,
            )
            | (PubdataMode::Validium, BatchDaInputMode::Rollup) => {
                panic!(
                    "Pubdata mode doesn't correspond to pricing mode from the l1. \
                    L1 mode: {:?}, configured pubdata mode: {:?}",
                    l1_state.da_input_mode, pubdata_mode
                );
            }
            _ => {}
        };
        if let (PubdataMode::Blobs | PubdataMode::Calldata, true) = (
            pubdata_mode,
            config.general_config.gateway_rpc_url.is_some(),
        ) {
            panic!(
                "Pubdata mode {:?} cannot be used when settling on Gateway",
                pubdata_mode
            );
        }
    }

    let genesis = Genesis::new(
        genesis_input_source.clone(),
        l1_state.diamond_proxy_l1.clone(),
        chain_id,
    );

    tracing::info!("Initializing BlockReplayStorage");

    let block_replay_storage = BlockReplayStorage::new(
        &config
            .general_config
            .rocks_db_path
            .join(BLOCK_REPLAY_WAL_DB_NAME),
        &genesis,
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

    let tree_at_genesis = MerkleTreeVersion {
        tree: tree_db,
        block: 0,
    };
    let (genesis_root_hash, genesis_root_leaves) = tree_at_genesis
        .root_info()
        .expect("Failed to get genesis root info");
    let tree_db = tree_at_genesis.tree;
    let tree_for_rpc = Arc::new(tree_db.clone());

    // todo: this can take a while; ideally committed batches should be loaded in the background
    //       and then `get()` method can be made async so that it waits for relevant batch to load
    let committed_batch_provider = CommittedBatchProvider::init(
        &l1_state,
        config.l1_watcher_config.max_blocks_to_process,
        || async {
            let genesis_state = genesis.state().await;
            load_genesis_stored_batch_info(genesis_state, genesis_root_hash, genesis_root_leaves)
                .await
                .unwrap()
        },
    )
    .await
    .expect("failed to init CommittedBatchProvider");

    let state = State::new(&config.general_config, &genesis).await;

    tracing::info!("Initializing mempools");
    let zk_provider_factory = ZkProviderFactory::new(state.clone(), repositories.clone(), chain_id);
    let l2_subpool = zksync_os_mempool::subpools::l2::in_memory(
        zk_provider_factory.clone(),
        config.mempool_config.clone().into(),
        config.tx_validator_config.clone().into(),
    );

    let (last_l1_committed_block, last_l1_proved_block, last_l1_executed_block) =
        commit_proof_execute_block_numbers(&l1_state, &committed_batch_provider).await;

    let node_startup_state = NodeStateOnStartup {
        node_role,
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

    if let Some(block_rebuild) = &config.sequencer_config.block_rebuild
        && node_role.is_main()
    {
        // The assertion is only relevant for the main node.
        // External node can be started at any point and doesn't have to be in sync with L1.
        // But the main node is expected to only produce blocks on top of committed L1 blocks,
        // as those can't be re-sequenced.
        assert!(
            block_rebuild.from_block > node_startup_state.last_l1_committed_block,
            "rebuild_from_block must be > last_l1_committed_block, got {} <= {}",
            block_rebuild.from_block,
            node_startup_state.last_l1_committed_block
        );
    }

    let finality_storage = Finality::new(FinalityStatus {
        last_committed_block: last_l1_committed_block,
        last_committed_batch: l1_state.last_committed_batch,
        last_executed_block: last_l1_executed_block,
        last_executed_batch: l1_state.last_executed_batch,
    });

    // `starting_block` - the block number to go through the pipeline.
    let starting_block = if node_startup_state.l1_state.last_committed_batch > 0 {
        // todo: ideally this should be searched through p2p networking instead of RPC
        //       but too many things depend on this being initialized here right now
        //       once refactored we can get rid of `main_node_rpc_url` config param
        let last_matching_block =
            if let Some(main_node_rpc_url) = &config.general_config.main_node_rpc_url {
                find_last_matching_main_node_block(&repositories, main_node_rpc_url)
                    .await
                    .expect("Failed to find last matching block with main node")
            } else {
                node_startup_state.repositories_persisted_block
            };
        // Some batches committed - starting from an already committed batch
        determine_starting_block(&config, &node_startup_state, &state, last_matching_block)
    } else {
        // No batches committed - starting from block/batch 1.
        1
    };

    tracing::info!(
        config.general_config.min_blocks_to_replay,
        config.general_config.force_starting_block_number,
        ?node_startup_state,
        starting_block,
        blocks_to_replay = node_startup_state.block_replay_storage_last_block + 1 - starting_block,
        "Node state on startup"
    );

    node_startup_state.assert_consistency();

    // Channel between NetworkService and Sequencer
    let (replay_sender, replays_for_sequencer) = tokio::sync::mpsc::channel(128);

    let ConsensusRuntimeParts {
        canonization_engine,
        leadership,
        ..
    } = loopback_consensus();
    if config.network_config.enabled {
        tracing::info!("initializing p2p networking");

        let network_service = NetworkService::new(
            config.network_config.clone().into(),
            ZksProtocolConfig {
                node_role,
                starting_block,
                // This will be gone once we migrate away from record overrides
                record_overrides: config
                    .sequencer_config
                    .en_replay_record_overrides
                    .iter()
                    .map(|(block_number, db_key)| RecordOverride {
                        block_number: *block_number,
                        db_key: db_key.clone(),
                    })
                    .collect(),
                replay_sender,
            },
            block_replay_storage.clone(),
            zk_provider_factory,
        )
        .await
        .expect("failed to create network service");
        network_service.run(&mut tasks, stop_receiver.clone());
    } else if node_role.is_main() {
        tracing::info!(
            "p2p networking is disabled; to enable set `network.enabled=true` and populate `network.secret_key`"
        );
    } else {
        panic!(
            "EN cannot run without p2p networking; to fix: \
            set `network.enabled=true` to enable p2p networking, \
            populate `network.secret_key` with a 256-bit ECDSA key (can be randomly generated locally), \
            populate `network.boot_nodes` with at least one known node from the chain. \
            See https://github.com/matter-labs/zksync-os-server/pull/873 for full rollout instructions."
        );
    }

    tracing::info!("Initializing L1 Watchers");
    tasks.spawn(
        L1CommitWatcher::create_watcher(
            config.l1_watcher_config.clone().into(),
            node_startup_state.l1_state.diamond_proxy_sl.clone(),
            committed_batch_provider.clone(),
            finality_storage.clone(),
        )
        .await
        .expect("failed to start L1 commit watcher")
        .run()
        .map(report_exit("L1 commit watcher")),
    );

    tasks.spawn(
        L1ExecuteWatcher::create_watcher(
            config.l1_watcher_config.clone().into(),
            node_startup_state.l1_state.diamond_proxy_sl.clone(),
            committed_batch_provider.clone(),
            finality_storage.clone(),
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

    let next_interop_event_index = first_replay_record
        .as_ref()
        .map_or(InteropRootsLogIndex::default(), |record| {
            record.starting_interop_event_index.clone()
        });

    let next_migration_number = first_replay_record
        .as_ref()
        .map_or(0, |record| record.starting_migration_number);
    let next_interop_fee_number = first_replay_record
        .as_ref()
        .map_or(0, |record| record.starting_interop_fee_number);

    let current_protocol_version = if let Some(record) = &first_replay_record {
        &record.protocol_version
    } else {
        &genesis.genesis_upgrade_tx().await.protocol_version
    };

    if config
        .sequencer_config
        .tx_validator
        .deployment_filter
        .enabled
    {
        let exec_version = ExecutionVersion::try_from(current_protocol_version)
            .expect("Cannot determine execution version");
        assert!(
            exec_version >= ExecutionVersion::V6,
            "Deployment filter requires execution version V6 or later (protocol >= v31.0), \
             but current protocol version {current_protocol_version} uses {exec_version:?}"
        );
    }

    let upgrade_subpool = UpgradeSubpool::new(current_protocol_version.clone());
    let sl_chain_id_subpool = SlChainIdSubpool::default();
    let interop_fee_subpool = InteropFeeSubpool::new(next_interop_fee_number);
    let interop_roots_subpool = InteropRootsSubpool::new(
        // todo: change to config.sequencer_config.interop_roots_per_tx when contracts are updated
        1,
    );

    // If we start from the very first block, we should start by sending upgrade tx for genesis.
    if starting_block == 1 {
        let genesis_upgrade = genesis.genesis_upgrade_tx().await;
        let upgrade_tx = UpgradeInfo {
            tx: Some(genesis_upgrade.tx.clone()),
            metadata: UpgradeMetadata {
                protocol_version: genesis_upgrade.protocol_version.clone(),
                timestamp: 0, // No restrictions on timestamp.
                force_preimages: genesis_upgrade.force_deploy_preimages.clone(),
            },
        };
        upgrade_subpool.insert(upgrade_tx).await;
    }

    if current_protocol_version >= &ProtocolSemanticVersion::new(0, 31, 0) {
        tasks.spawn(
            GatewayMigrationWatcher::create_watcher(
                node_startup_state.l1_state.diamond_proxy_l1.clone(),
                node_startup_state.l1_state.bridgehub_l1.clone(),
                chain_id,
                node_startup_state.l1_state.l1_chain_id,
                config.general_config.gateway_chain_id,
                next_migration_number,
                config.l1_watcher_config.clone().into(),
                sl_chain_id_subpool.clone(),
            )
            .await
            .expect("failed to start gateway migration watcher")
            .run()
            .map(report_exit("gateway migration watcher")),
        );

        if config.general_config.gateway_rpc_url.is_some() {
            tasks.spawn(
                InteropWatcher::create_watcher(
                    node_startup_state.l1_state.bridgehub_sl.clone(),
                    config.l1_watcher_config.clone().into(),
                    next_interop_event_index.clone(),
                    interop_roots_subpool.clone(),
                )
                .await
                .expect("failed to start L1 interop roots watcher")
                .run()
                .map(report_exit("L1 interop roots watcher")),
            );
        }
    }

    let l1_subpool = L1Subpool::new(10);
    tasks.spawn(
        L1TxWatcher::create_watcher(
            config.l1_watcher_config.clone().into(),
            node_startup_state.l1_state.diamond_proxy_l1.clone(),
            node_startup_state.l1_state.diamond_proxy_sl.clone(),
            l1_subpool.clone(),
            next_l1_priority_id,
        )
        .await
        .expect("failed to start L1 transaction watcher")
        .run()
        .map(report_exit("L1 transaction watcher")),
    );

    // Transaction acceptance state - tracks whether we're accepting new transactions
    // Main nodes: accepts, but may switch to reject when `sequencer_max_blocks_to_produce` blocks are produced
    // External nodes: always accepts, but may be rejected on the main node side during forwarding
    let (tx_acceptance_state_sender, tx_acceptance_state_receiver) =
        watch::channel(TransactionAcceptanceState::Accepting);

    let main_node_provider = if let Some(url) = config.general_config.main_node_rpc_url.as_ref() {
        Some(
            ProviderBuilder::new()
                .connect(url)
                .await
                .expect("could not connect to main node RPC")
                .erased(),
        )
    } else {
        None
    };

    let (last_constructed_block_ctx_sender, last_constructed_block_ctx_receiver) =
        watch::channel(None);

    tracing::info!("Initializing pubdata price provider");
    // Channels for GasAdjuster->BlockContextProvider communication.
    let (pubdata_price_sender, pubdata_price_receiver) = watch::channel(None);
    let (blob_fill_ratio_sender, blob_fill_ratio_receiver) = watch::channel(None);
    // Channel for Batcher->GasAdjuster communication. Batcher send sidecar to gas adjuster to estimate blob fill ratio.
    let (sidecar_sender, sidecar_receiver) = tokio::sync::mpsc::channel(10);
    if node_role.is_main() {
        let pubdata_mode = config
            .l1_sender_config
            .pubdata_mode
            .expect("l1_sender_pubdata_mode must be set on the Main Node");
        let gas_adjuster_config = gas_adjuster_config(
            config.gas_adjuster_config.clone(),
            pubdata_mode,
            config.l1_sender_config.max_priority_fee_per_gas.0,
        );
        let gas_adjuster = GasAdjuster::new(
            sl_provider.clone().erased(),
            gas_adjuster_config,
            pubdata_price_sender,
            blob_fill_ratio_sender,
            sidecar_receiver,
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

    let (token_price_sender, token_price_receiver) = watch::channel(None);
    let interop_fee_token_price_receiver = token_price_receiver.clone();
    let previous_block_fee_params = if starting_block == 1 {
        None
    } else {
        let prev_record = block_replay_storage
            .get_replay_record(starting_block - 1)
            .unwrap_or_else(|| {
                panic!(
                    "Missing replay record for block `starting_block - 1` = {}",
                    starting_block - 1
                )
            });
        Some(FeeParams {
            eip1559_basefee: prev_record.block_context.eip1559_basefee,
            native_price: prev_record.block_context.native_price,
            pubdata_price: prev_record.block_context.pubdata_price,
        })
    };

    // todo: `BlockContextProvider` initialization and its dependencies
    // should be moved to `sequencer`
    let fee_provider = FeeProvider::new(
        config.fee_config.clone().into(),
        previous_block_fee_params,
        pubdata_price_receiver,
        blob_fill_ratio_receiver,
        token_price_receiver,
        config.l1_sender_config.pubdata_mode,
    );

    let pool = Pool::new(
        upgrade_subpool.clone(),
        sl_chain_id_subpool,
        interop_fee_subpool.clone(),
        interop_roots_subpool,
        l1_subpool,
        l2_subpool.clone(),
    );
    let block_context_provider = BlockContextProvider::new(
        next_l1_priority_id,
        next_interop_event_index,
        next_migration_number,
        next_interop_fee_number,
        pool,
        block_hashes_for_next_block,
        previous_block_timestamp,
        starting_block,
        config.sequencer_config.block_time,
        config.sequencer_config.max_transactions_in_block,
        chain_id,
        config.sequencer_config.block_gas_limit,
        config.sequencer_config.block_pubdata_limit_bytes,
        // We set the value to the same as for the batch, since it should be enforced by batcher, but don't want to exceed it for the block
        config.batcher_config.interop_roots_per_batch_limit,
        config.sequencer_config.service_block_delay,
        current_protocol_version.clone(),
        config.sequencer_config.fee_collector_address,
        last_constructed_block_ctx_sender,
        fee_provider,
    );

    // ========== Start L1 Upgrade Watcher ===========

    tasks.spawn(
        L1UpgradeTxWatcher::create_watcher(
            config.l1_watcher_config.clone().into(),
            node_startup_state.l1_state.diamond_proxy_l1.clone(),
            node_startup_state.l1_state.diamond_proxy_sl.clone(),
            bytecode_supplier_address,
            current_protocol_version.clone(),
            upgrade_subpool,
        )
        .await
        .expect("failed to start L1 upgrade transaction watcher")
        .run()
        .map(report_exit("L1 upgrade transaction watcher")),
    );

    // ========== Start L1 Persist Batch Watcher ===========

    let persistent_batch_storage =
        ExecutedBatchStorage::new(&config.general_config.rocks_db_path.join(BATCH_DB_NAME));
    let rpc_storage = RpcStorage::new(
        repositories.clone(),
        block_replay_storage.clone(),
        finality_storage.clone(),
        persistent_batch_storage.clone(),
        state.clone(),
        tree_for_rpc,
    );
    tasks.spawn(
        L1PersistBatchWatcher::create_watcher(
            config.l1_watcher_config.clone().into(),
            node_startup_state.l1_state.diamond_proxy_sl.clone(),
            persistent_batch_storage.clone(),
        )
        .await
        .expect("failed to start L1 batch persist watcher")
        .run()
        .map(report_exit("L1 batch persist watcher")),
    );

    // ========== Start Sequencer ===========
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

    if node_role.is_main() {
        let external_price_api_client_config = config
            .external_price_api_client_config
            .clone()
            .expect("external_price_api_client config must be set for Main Node");
        let gateway_diamond_proxy = if l1_state.l1_chain_id != l1_state.sl_chain_id {
            Some(
                l1_state
                    .bridgehub_l1
                    .zk_chain_by_chain_id(l1_state.sl_chain_id)
                    .await
                    .expect("Failed to get gateway_diamond_proxy"),
            )
        } else {
            None
        };
        let mut base_token_price_updater = BaseTokenPriceUpdater::new(
            l1_state.diamond_proxy_l1.clone(),
            gateway_diamond_proxy,
            l1_provider.clone(),
            base_token_price_updater_config(
                &config.base_token_price_updater_config,
                &config.l1_sender_config,
            ),
            external_price_api_client_config.into(),
            token_price_sender,
        )
        .await
        .expect("Failed to initialize BaseTokenPriceUpdater");
        let stop_receiver_ = stop_receiver.clone();
        tasks.spawn(async move {
            base_token_price_updater
                .run(stop_receiver_)
                .map(|_| tracing::warn!("base_token_price_updater.run() unexpectedly exited"))
                .await;
        });
    }

    if node_role.is_main()
        && config.general_config.gateway_rpc_url.is_some()
        && current_protocol_version >= &ProtocolSemanticVersion::new(0, 31, 0)
    {
        let eth_call_handler = EthCallHandler::new(
            config.rpc_config.clone().into(),
            rpc_storage.clone(),
            chain_id,
            last_constructed_block_ctx_receiver.clone(),
        );
        let mut interop_fee_updater = InteropFeeUpdater::new(
            eth_call_handler,
            sl_provider.clone().erased(),
            interop_fee_subpool,
            interop_fee_token_price_receiver,
            InteropFeeUpdaterConfig {
                polling_interval: config.interop_fee_updater_config.polling_interval,
                update_deviation_percentage: config
                    .interop_fee_updater_config
                    .update_deviation_percentage,
            },
        );
        let stop_receiver_ = stop_receiver.clone();
        tasks.spawn(async move {
            interop_fee_updater
                .run(stop_receiver_)
                .map(|_| tracing::warn!("interop_fee_updater.run() unexpectedly exited"))
                .await;
        });
    }

    if node_role.is_main() {
        // Main Node
        run_main_node_pipeline(
            &config,
            sl_provider.clone(),
            node_startup_state,
            block_replay_storage.clone(),
            &mut tasks,
            state.clone(),
            starting_block,
            repositories.clone(),
            block_context_provider,
            tree_db,
            finality_storage.clone(),
            chain_id,
            tx_acceptance_state_sender,
            sidecar_sender,
            committed_batch_provider.clone(),
            canonization_engine,
            leadership,
        )
        .await;
    } else {
        // External Node
        run_en_pipeline(
            &config,
            replays_for_sequencer,
            committed_batch_provider.clone(),
            node_startup_state,
            block_replay_storage.clone(),
            &mut tasks,
            block_context_provider,
            state.clone(),
            tree_db,
            repositories.clone(),
            finality_storage.clone(),
            stop_receiver.clone(),
            tx_acceptance_state_sender,
            chain_id,
        )
        .await;
    };

    // ======== Start Status Server ========
    if config.status_server_config.enabled {
        tasks.spawn(
            run_status_server(
                config.status_server_config.address.clone(),
                stop_receiver.clone(),
            )
            .map(report_exit("Status server")),
        );
    }

    // =========== Start JSON RPC ========

    tasks.spawn(
        run_jsonrpsee_server(
            config.rpc_config.into(),
            chain_id,
            bridgehub_address,
            bytecode_supplier_address,
            rpc_storage,
            l2_subpool,
            genesis_input_source,
            tx_acceptance_state_receiver,
            last_constructed_block_ctx_receiver,
            main_node_provider,
            gateway_provider.map(|p| p.erased()),
        )
        .map(report_exit("JSON-RPC server")),
    );
    let startup_time = process_started_at.elapsed();
    GENERAL_METRICS.startup_time[&"total"].set(startup_time.as_secs_f64());
    tracing::info!("All components initialized in {startup_time:?}");
    tasks.join_next().await;
    tracing::info!("One of the subsystems exited - exiting process.");
}

#[allow(clippy::too_many_arguments)]
async fn run_main_node_pipeline(
    config: &Config,
    sl_provider: FillProvider<
        impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet> + 'static,
        impl Provider<Ethereum> + Clone + 'static,
    >,
    node_state_on_startup: NodeStateOnStartup,
    block_replay_storage: impl WriteReplay + Clone,
    tasks: &mut JoinSet<()>,
    state: impl ReadStateHistory + WriteState + Clone,
    starting_block: u64,
    repositories: impl WriteRepository + Clone,
    block_context_provider: BlockContextProvider<impl L2Subpool>,
    tree: MerkleTree<RocksDBWrapper>,
    finality: impl ReadFinality + Clone,
    chain_id: u64,
    tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
    sidecar_sender: tokio::sync::mpsc::Sender<BlobTransactionSidecar>,
    committed_batch_provider: CommittedBatchProvider,
    canonization_engine: BlockCanonizationEngine,
    leadership: LeadershipSignal,
) {
    let pubdata_mode = config
        .l1_sender_config
        .pubdata_mode
        .expect("l1_sender_pubdata_mode must be set on the Main Node");
    let priority_tree_db_path = config
        .general_config
        .rocks_db_path
        .join(PRIORITY_TREE_DB_NAME);
    let internal_config_manager = init_and_report_internal_config_manager(
        config
            .general_config
            .rocks_db_path
            .join(INTERNAL_CONFIG_FILE_NAME),
    );

    let (replays_to_execute_sender, replays_to_execute) = tokio::sync::mpsc::channel(8);

    let pipeline = Pipeline::new()
        .pipe(ConsensusNodeCommandSource {
            block_replay_storage: block_replay_storage.clone(),
            starting_block,
            rebuild_options: config
                .sequencer_config
                .block_rebuild
                .clone()
                .map(Into::into),
            replays_to_execute,
            leadership,
        })
        .pipe(BlockExecutor {
            block_context_provider,
            state: state.clone(),
            config: config.into(),
            tx_acceptance_state_sender,
        })
        .pipe(BlockCanonizer {
            consensus: canonization_engine,
            canonized_blocks_for_execution: replays_to_execute_sender,
        })
        .pipe(BlockApplier {
            state: state.clone(),
            replay: block_replay_storage.clone(),
            repositories: repositories.clone(),
            config: config.into(),
        })
        .pipe_opt(
            config
                .sequencer_config
                .revm_consistency_checker_enabled
                .then(|| {
                    RevmConsistencyChecker::new(
                        state.clone(),
                        internal_config_manager.clone(),
                        config
                            .sequencer_config
                            .revm_consistency_checker_revert_on_divergence,
                    )
                }),
        )
        .pipe(TreeManager { tree: tree.clone() });
    tracing::info!("Initializing ProofStorage");
    let proof_storage = ProofStorage::new(config.prover_api_config.proof_storage.clone())
        .await
        .expect("Failed to initialize ProofStorage");

    let (fri_proving_step, fri_job_manager) = FriProvingPipelineStep::new(
        proof_storage.clone(),
        node_state_on_startup.l1_state.last_proved_batch,
        config.prover_api_config.fri_job_timeout,
        config.prover_api_config.max_assigned_batch_range,
    );

    let (snark_proving_step, snark_job_manager) = SnarkProvingPipelineStep::new(
        config.prover_api_config.max_fris_per_snark,
        node_state_on_startup.l1_state.last_proved_batch,
        config.prover_api_config.snark_job_timeout,
        config.prover_api_config.max_assigned_batch_range,
    );

    if config.prover_api_config.enabled {
        tasks.spawn(
            prover_server::run(
                fri_job_manager.clone(),
                snark_job_manager.clone(),
                proof_storage.clone(),
                config.prover_api_config.address.clone(),
            )
            .map(report_exit("prover_server_job")),
        );
    }

    if config.prover_api_config.fake_fri_provers.enabled {
        run_fake_fri_provers(&config.prover_api_config, tasks, fri_job_manager);
    }

    if config.prover_api_config.fake_snark_provers.enabled {
        run_fake_snark_provers(&config.prover_api_config, tasks, snark_job_manager);
    }

    let pipeline = pipeline
        .pipe(ProverInputGenerator {
            enable_logging: config.prover_input_generator_config.logging_enabled,
            maximum_in_flight_blocks: config
                .prover_input_generator_config
                .maximum_in_flight_blocks,
            read_state: state.clone(),
            pubdata_mode,
        })
        .pipe(Batcher {
            startup_config: BatcherStartupConfig {
                last_committed_batch: node_state_on_startup.l1_state.last_committed_batch,
                last_executed_batch: node_state_on_startup.l1_state.last_executed_batch,
                last_persisted_block: node_state_on_startup.block_replay_storage_last_block,
            },
            chain_id,
            sl_chain_id: node_state_on_startup.l1_state.sl_chain_id,
            chain_address_sl: node_state_on_startup.l1_state.diamond_proxy_address_sl(),
            pubdata_limit_bytes: config.sequencer_config.block_pubdata_limit_bytes,
            batcher_config: config.batcher_config.clone(),
            pubdata_mode,
            sidecar_sender,
            committed_batch_provider: committed_batch_provider.clone(),
            read_state: state.clone(),
        })
        .pipe(BatchVerificationPipelineStep::new(
            config.batch_verification_config.clone().into(),
            node_state_on_startup.l1_state.clone(),
            node_state_on_startup.l1_state.last_committed_batch,
        ))
        .pipe(fri_proving_step)
        .pipe(GaplessCommitter {
            next_expected_batch_number: node_state_on_startup.l1_state.last_executed_batch + 1,
            last_committed_batch_number: node_state_on_startup.l1_state.last_committed_batch,
            proof_storage,
            batch_verification_l1_config: node_state_on_startup.l1_state.batch_verification.clone(),
        })
        .pipe(UpgradeGatekeeper::new(
            node_state_on_startup.l1_state.diamond_proxy_sl.clone(),
        ))
        .pipe(L1Sender::<_, _, CommitCommand> {
            provider: sl_provider.clone(),
            config: config.l1_sender_config.clone().into(),
            to_address: node_state_on_startup.l1_state.validator_timelock_sl,
            gateway: config.general_config.gateway_rpc_url.is_some(),
        })
        .pipe(snark_proving_step)
        .pipe(GaplessL1ProofSender::new(
            node_state_on_startup.l1_state.last_executed_batch + 1,
        ))
        .pipe(L1Sender::<_, _, ProofCommand> {
            provider: sl_provider.clone(),
            config: config.l1_sender_config.clone().into(),
            to_address: node_state_on_startup.l1_state.validator_timelock_sl,
            gateway: config.general_config.gateway_rpc_url.is_some(),
        })
        .pipe(
            PriorityTreePipelineStep::new(
                block_replay_storage.clone(),
                &priority_tree_db_path,
                finality,
                committed_batch_provider,
            )
            .await
            .unwrap(),
        )
        .pipe(L1Sender {
            provider: sl_provider,
            config: config.l1_sender_config.clone().into(),
            to_address: node_state_on_startup.l1_state.validator_timelock_sl,
            gateway: config.general_config.gateway_rpc_url.is_some(),
        })
        .pipe(BatchSink::new(internal_config_manager));

    tracing::info!("Launching pipeline");
    pipeline.spawn(tasks);
}

/// Only for EN - we still populate channels destined for the batcher subsystem -
/// need to drain them to not get stuck
#[allow(clippy::too_many_arguments)]
async fn run_en_pipeline(
    config: &Config,
    replays_for_sequencer: tokio::sync::mpsc::Receiver<ReplayRecord>,
    committed_batch_provider: CommittedBatchProvider,
    node_state_on_startup: NodeStateOnStartup,
    block_replay_storage: impl WriteReplay + Clone,
    tasks: &mut JoinSet<()>,
    block_context_provider: BlockContextProvider<impl L2Subpool>,
    state: impl ReadStateHistory + WriteState + Clone,
    tree: MerkleTree<RocksDBWrapper>,
    repositories: impl WriteRepository + Clone,
    finality: impl ReadFinality + Clone,
    stop_receiver: watch::Receiver<bool>,
    tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
    chain_id: u64,
) {
    let internal_config_manager = init_and_report_internal_config_manager(
        config
            .general_config
            .rocks_db_path
            .join(INTERNAL_CONFIG_FILE_NAME),
    );

    Pipeline::new()
        .pipe(ExternalNodeCommandSource {
            replays_for_sequencer,
            up_to_block: config.sequencer_config.en_sync_up_to_block,
            stop_receiver: stop_receiver.clone(),
        })
        .pipe(BlockExecutor {
            block_context_provider,
            state: state.clone(),
            config: config.into(),
            tx_acceptance_state_sender,
        })
        .pipe(BlockApplier {
            state: state.clone(),
            replay: block_replay_storage.clone(),
            repositories: repositories.clone(),
            config: config.into(),
        })
        .pipe_opt(
            config
                .sequencer_config
                .revm_consistency_checker_enabled
                .then(|| {
                    RevmConsistencyChecker::new(
                        state.clone(),
                        internal_config_manager.clone(),
                        config
                            .sequencer_config
                            .revm_consistency_checker_revert_on_divergence,
                    )
                }),
        )
        .pipe(TreeManager { tree: tree.clone() })
        .pipe_if(
            config.batch_verification_config.client_enabled,
            BatchVerificationClient::new(
                chain_id,
                node_state_on_startup.l1_state.diamond_proxy_address_sl(),
                config.batch_verification_config.connect_address.clone(),
                config.batch_verification_config.signing_key.clone(),
                finality.clone(),
                node_state_on_startup.l1_state.clone(),
                state.clone(),
            ),
            NoOpSink::new(),
        )
        .spawn(tasks);

    // Run Priority Tree tasks for EN - not part of the pipeline.
    if config.general_config.run_priority_tree {
        let priority_tree_en_step = PriorityTreeENStep::new(
            block_replay_storage,
            Path::new(
                &config
                    .general_config
                    .rocks_db_path
                    .join(PRIORITY_TREE_DB_NAME),
            ),
            finality.clone(),
            committed_batch_provider,
        )
        .await
        .unwrap();

        tasks.spawn(
            priority_tree_en_step
                .run()
                .map(report_exit("priority_tree_en")),
        );
    }
    tasks.spawn(
        clear_failing_block_config_task(finality, internal_config_manager)
            .map(report_exit("clear_failing_block_config_task")),
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

fn init_and_report_internal_config_manager(
    internal_config_path: std::path::PathBuf,
) -> InternalConfigManager {
    let internal_config_manager = InternalConfigManager::new(internal_config_path)
        .expect("Failed to initialize InternalConfigManager");

    // Report blacklisted addresses metric
    let internal_config = internal_config_manager
        .read_config()
        .expect("Failed to read internal config");
    GENERAL_METRICS
        .blacklisted_addresses_count
        .set(internal_config.l2_signer_blacklist.len());

    internal_config_manager
}

/// Warns when the main node's batch verification server threshold is lower than the
/// threshold configured on L1.
///
/// This is a startup sanity check only: the pipeline later enforces the effective threshold by
/// taking the max(server.threshold, l1.threshold).
///
/// In practice, it means that the server operator expectation and the L1 state are mismatched.
fn check_batch_verification_mismatch(
    server_config: &config::BatchVerificationConfig,
    l1_config: &BatchVerificationSL,
) -> bool {
    if !server_config.server_enabled {
        return false;
    }

    let l1_threshold = match l1_config {
        BatchVerificationSL::Enabled(config) => config.threshold,
        BatchVerificationSL::Disabled => return false,
    };

    if server_config.threshold < l1_threshold {
        tracing::warn!(
            configured_threshold = server_config.threshold,
            l1_threshold,
            "Batch verification server threshold is lower than the L1 threshold; consider increasing the server threshold"
        );
        return true;
    }
    false
}

async fn commit_proof_execute_block_numbers(
    l1_state: &L1State,
    committed_batch_provider: &CommittedBatchProvider,
) -> (u64, u64, u64) {
    let last_committed_block = if l1_state.last_committed_batch == 0 {
        0
    } else {
        committed_batch_provider
            .get(l1_state.last_committed_batch)
            .expect("last committed batch was not discovered on L1")
            .last_block_number()
    };

    // only used to log on node startup
    let last_proved_block = if l1_state.last_proved_batch == 0 {
        0
    } else {
        committed_batch_provider
            .get(l1_state.last_proved_batch)
            .expect("last proved batch was not discovered on L1")
            .last_block_number()
    };

    let last_executed_block = if l1_state.last_executed_batch == 0 {
        0
    } else {
        committed_batch_provider
            .get(l1_state.last_executed_batch)
            .expect("last executed batch was not discovered on L1")
            .last_block_number()
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
        timeout_frequency = ?config.fake_fri_provers.timeout_frequency,
        "Initializing fake FRI provers"
    );
    let fake_provers_pool = FakeFriProversPool::new(
        fri_job_manager.clone(),
        config.fake_fri_provers.workers,
        config.fake_fri_provers.compute_time,
        config.fake_fri_provers.min_age,
        config.fake_fri_provers.timeout_frequency,
    );
    tasks.spawn(
        fake_provers_pool
            .run()
            .map(report_exit("fake_fri_provers_task_optional")),
    );
}

/// Determines the block for node to start from.
///
/// Panics if no batches are committed to L1 yet.
fn determine_starting_block(
    config: &Config,
    node_startup_state: &NodeStateOnStartup,
    state: &impl ReadStateHistory,
    last_matching_block: BlockNumber,
) -> BlockNumber {
    assert!(
        node_startup_state.l1_state.last_committed_batch > 0,
        "No batches committed to L1 yet - start with block/batch 1"
    );

    let desired_starting_block = if let Some(forced_starting_block_number) =
        config.general_config.force_starting_block_number
    {
        forced_starting_block_number
    } else {
        // Start with the oldest block from:
        let want_to_start_from = [
            // To ensure consistency/correctness, we want to replay at least `config.min_blocks_to_replay` blocks
            node_startup_state
                .block_replay_storage_last_block
                .saturating_sub(config.general_config.min_blocks_to_replay as u64),
            // We need to replay old unexecuted blocks to rebuild and execute the batches they are in
            node_startup_state.last_l1_executed_block + 1,
            // Repositories' persistence may have fallen behind - we need to replay blocks to rebuild it
            node_startup_state.repositories_persisted_block + 1,
            // In the current tree implementation this will always be ahead of `last_l1_executed_block`,
            // but this may change if we make tree persistence async (like elsewhere)
            node_startup_state.tree_last_block + 1,
            // For compacted state, we need to replay all blocks that were not persisted yet.
            // For FullDiffs state (default) - this is always ahead of `last_l1_executed_block`.
            state.block_range_available().end() + 1,
            // If block rebuild (aka block reversion) is configured, we should ensure we replay
            // all the blocks we are rebuilding
            config
                .sequencer_config
                .block_rebuild
                .as_ref()
                .map_or(u64::MAX, |block_rebuild| block_rebuild.from_block),
        ]
        .into_iter()
        .min()
        .unwrap()
        // We don't execute the genesis block (number 0) - the earliest we can start is `0`
        .max(1);

        if last_matching_block + 1 < want_to_start_from {
            tracing::warn!(
                last_matching_block,
                want_to_start_from,
                "Node's blocks diverged from main node's blocks. Starting from last matching block + 1."
            );
        }

        (last_matching_block + 1).min(want_to_start_from)
    };

    if desired_starting_block < state.block_range_available().start() + 1 {
        // This may only happen with Compacted State. This means that the block we want to rerun was already compacted.
        // This can be fixed by manually removing the storage persistence - which will force the node to start from block 1.

        // Alternatively, we can clear storage programmatically here and start from 1 - this is not currently implemented
        panic!(
            "Cannot start: desired_starting_block < state.block_range_available().start() + 1: {} < {}",
            desired_starting_block,
            state.block_range_available().start() + 1
        );
    }

    desired_starting_block
}

/// Finds the last block number where the local node's block hash matches the main node's block hash.
async fn find_last_matching_main_node_block(
    repo: &RepositoryManager,
    main_node_rpc_url: &str,
) -> anyhow::Result<u64> {
    async fn check(
        repo: &RepositoryManager,
        main_node_client: &HttpClient,
        block_number: u64,
    ) -> anyhow::Result<bool> {
        let local_hash = repo
            .get_block_by_number(block_number)?
            .map(|b| b.hash())
            .with_context(|| format!("Local node is missing block {block_number}"))?;
        if let Some(remote_block) = main_node_client
            .block_by_number(block_number.into(), false)
            .await?
        {
            Ok(local_hash == remote_block.hash())
        } else {
            // Main node is missing this block in RPC, assume there is a divergence.
            //
            // If we happen to query main node during restart it might not have this block in RPC
            // yet but have it in replay storage. Should still be fine to assume there is a divergence
            // in such cases. Ideally, we should be able to query main node's replay state through
            // interactive replay transport instead.
            Ok(false)
        }
    }

    let main_node_rpc_client =
        jsonrpsee::http_client::HttpClientBuilder::new().build(main_node_rpc_url)?;
    let last_block = repo.get_latest_block();
    // Check last block first. Unless there was a reorg recently, this should return quickly.
    if check(repo, &main_node_rpc_client, last_block).await? {
        return Ok(last_block);
    }
    if !check(repo, &main_node_rpc_client, 0).await? {
        panic!("Genesis block mismatch between EN and main node");
    }

    // Binary search for the last matching block.
    let mut left = 0u64;
    let mut right = last_block;
    while left < right {
        #[allow(clippy::manual_div_ceil)]
        let mid = (left + right + 1) / 2;
        if check(repo, &main_node_rpc_client, mid).await? {
            left = mid;
        } else {
            right = mid - 1;
        }
    }
    Ok(left)
}

#[cfg(test)]
mod tests {
    use super::check_batch_verification_mismatch;
    use crate::config::BatchVerificationConfig;
    use alloy::primitives::address;
    use zksync_os_contract_interface::l1_discovery::{
        BatchVerificationSL, BatchVerificationSLConfig,
    };

    #[test]
    fn test_batch_verification_is_disabled_on_server() {
        let server_config = BatchVerificationConfig::default();
        let l1_config = BatchVerificationSL::Enabled(BatchVerificationSLConfig {
            threshold: 0,
            validators: vec![address!("0x0000000000000000000000000000000000000001")],
        });
        let warned = check_batch_verification_mismatch(&server_config, &l1_config);
        assert!(!warned);
    }

    #[test]
    fn test_batch_verification_is_disabled_on_l1() {
        let config = BatchVerificationConfig {
            server_enabled: true,
            ..Default::default()
        };
        let warned = check_batch_verification_mismatch(&config, &BatchVerificationSL::Disabled);
        assert!(!warned);
    }

    #[test]
    fn test_batch_verification_is_mismatched() {
        let server_config = BatchVerificationConfig {
            server_enabled: true,
            threshold: 2,
            ..Default::default()
        };
        let l1_config = BatchVerificationSL::Enabled(BatchVerificationSLConfig {
            threshold: 3,
            validators: vec![
                address!("0x0000000000000000000000000000000000000001"),
                address!("0x0000000000000000000000000000000000000002"),
                address!("0x0000000000000000000000000000000000000003"),
                address!("0x0000000000000000000000000000000000000004"),
            ],
        });
        let warned = check_batch_verification_mismatch(&server_config, &l1_config);

        assert!(warned);
    }

    #[test]
    fn test_batch_verification_happy_path() {
        let server_config = BatchVerificationConfig {
            server_enabled: true,
            threshold: 3,
            ..Default::default()
        };
        let l1_config = BatchVerificationSL::Enabled(BatchVerificationSLConfig {
            threshold: 2,
            validators: vec![
                address!("0x0000000000000000000000000000000000000001"),
                address!("0x0000000000000000000000000000000000000002"),
                address!("0x0000000000000000000000000000000000000003"),
            ],
        });
        let warned = check_batch_verification_mismatch(&server_config, &l1_config);

        assert!(!warned);
    }
}
