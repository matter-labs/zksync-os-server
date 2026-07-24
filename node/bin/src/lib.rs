#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]
mod acceptance;
mod batch_sink;
pub mod batcher;
mod chain_fingerprint;
pub mod chain_tip;
mod command_source;
pub mod config;
pub mod consensus;
pub mod default_protocol_version;
mod en_remote_config;
mod init_tx_forwarder;
mod l1_revert;
mod main_node_client;
mod node_state_on_startup;
mod ports;
mod priority_tree_pipeline_step;
pub mod prover_api;
mod prover_block;
mod prover_input_generator;
mod provider;
mod state_initializer;
pub mod tree_manager;
pub mod truncate;
pub mod util;

use crate::batch_sink::{BatchSink, NoOpSink, clear_failing_block_config_task};
use crate::batcher::{Batcher, BatcherStartupConfig, util::load_genesis_stored_batch_info};
use crate::command_source::{
    ConsensusCommittedSource, ConsensusNodeCommandSource, ExternalNodeCommandSource, RebuildOptions,
};
use crate::config::{
    Config, ProverApiConfig, RebuildConfig, base_token_price_updater_config, gas_adjuster_config,
    report_static_config_metrics,
};
use crate::en_remote_config::load_remote_config;
use crate::init_tx_forwarder::{build_round_robin_tx_forwarder, build_static_tx_forwarder};
use crate::l1_revert::revert_l1_on_startup;
use crate::main_node_client::MainNodeClient;
use crate::node_state_on_startup::NodeStateOnStartup;
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
use crate::provider::{ProviderKind, build_node_provider};
use crate::state_initializer::StateInitializer;
use crate::tree_manager::TreeManager;
use alloy::consensus::BlobTransactionSidecar;
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::Provider;
use anyhow::Context;
use priority_tree_pipeline_step::PriorityTreePipelineStep;
use reth_tasks::Runtime;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::watch;
use zksync_os_backpressure::{BackpressureMonitor, PipelineSnapshot, PipelineTracker};
use zksync_os_base_token_adjuster::{BaseTokenPriceHandle, BaseTokenPriceUpdater};
use zksync_os_batch_verification::{
    BatchVerificationConfig as BatchVerificationPolicyConfig, BatchVerificationPipelineStep,
    BatchVerificationResponder, effective_verification_policy,
};
use zksync_os_contract_interface::l1_discovery::{BatchVerificationSL, L1State};
use zksync_os_contract_interface::models::BatchDaInputMode;
use zksync_os_gas_adjuster::GasAdjuster;
use zksync_os_genesis::{FileGenesisInputSource, Genesis, GenesisInputSource};
use zksync_os_internal_config::InternalConfigManager;
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_l1_sender::commands::execute::ExecuteCommand;
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_l1_sender::pipeline_component::L1Sender;
use zksync_os_l1_sender::upgrade_gatekeeper::UpgradeGatekeeper;
use zksync_os_l1_watcher::L1PersistBatchWatcher;
use zksync_os_l1_watcher::{
    CommittedBatchProvider, L1CommitWatcher, L1ExecuteWatcher, L1FinalizedExecuteWatcher,
    L1RevertWatcher,
};
use zksync_os_mempool::Pool;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_merkle_tree::{MerkleTree, RocksDBWrapper};
use zksync_os_metadata::NODE_VERSION;
use zksync_os_network::RecordOverride;
use zksync_os_network::VerifyBatch;
use zksync_os_network::protocol::{
    ExternalNodeProtocolConfig, ExternalNodeVerifierConfig, MainNodeProtocolConfig,
    ZksProtocolConfig,
};
use zksync_os_network::service::{NetworkService, PeerVerifyBatch, PeerVerifyBatchResult};
use zksync_os_observability::GENERAL_METRICS;
use zksync_os_pipeline::Pipeline;
use zksync_os_priority_tree::PriorityTreeManager;
use zksync_os_provider::NodeProvider;
use zksync_os_replay_archive::{
    ReplayArchiveGateComponent, ReplayArchiver, ReplayArchivingWriteReplay, init_replay_archive,
};
use zksync_os_reth_compat::provider::ZkProviderFactory;
use zksync_os_revm_consistency_checker::node::RevmConsistencyChecker;
use zksync_os_rpc::RpcStorage;
use zksync_os_sequencer::execution::block_context_provider::BlockContextProvider;
use zksync_os_sequencer::execution::{
    BlockApplier, BlockCanonization, BlockCanonizer, BlockExecutor, FeeProvider, LeadershipSignal,
    NoopCanonization,
};
use zksync_os_sequencer::model::blocks::BlockPayload;
use zksync_os_status_server::{StatusServerState, run_status_server};
use zksync_os_storage::db::{BlockReplayStorage, ExecutedBatchStorage};
use zksync_os_storage::in_memory::Finality;
use zksync_os_storage::lazy::RepositoryManager;
use zksync_os_storage_api::{
    FinalityStatus, ReadFinality, ReadReplay, ReadRepository, ReadStateHistory, ReplayRecord,
    WriteReplay, WriteRepository, WriteState,
};
use zksync_os_types::{ExecutionVersion, NodeRole, PubdataMode, TransactionAcceptanceState};

use ports::BoundListeners;
pub use ports::ServerPorts;

/// Directory name of the write-ahead log inside `general.rocks_db_path`. Public so
/// tooling (and the test harness) can locate a *stopped* node's WAL — e.g. to read
/// the drained tip during a migration.
pub const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const STATE_TREE_DB_NAME: &str = "tree";
const PRIORITY_TREE_DB_NAME: &str = "priority_txs_tree";
const REPOSITORY_DB_NAME: &str = "repository";
const BATCH_DB_NAME: &str = "batch";
pub const INTERNAL_CONFIG_FILE_NAME: &str = "internal_config.json";

/// What `run` hands back to its embedder — the binary's `main`, or a test
/// harness. The embedder owns the reaction to a reached cutover: the
/// production binary shuts down so its supervisor restarts it into consensus;
/// a test harness restarts the node itself.
pub struct LaunchedNode {
    pub ports: ServerPorts,
    /// Flips to `true` when the write-ahead log reaches the scheduled
    /// consensus anchor. `None` when no cutover is pending on this boot.
    pub scheduled_cutover_reached: Option<watch::Receiver<bool>>,
}

/// With consensus enabled, decides whether this boot happens before the
/// scheduled cutover: `Some(anchor)` when the local write-ahead log has not
/// reached `consensus.genesis_height` yet. The log is opened read-only here
/// and dropped again before the node's real storage initialization.
fn scheduled_cutover_pending(config: &Config) -> Option<u64> {
    if !config.consensus_config.enabled {
        return None;
    }
    let anchor = config.consensus_config.genesis_height;
    if anchor == 0 {
        // Consensus from the chain's genesis — there is no pre-cutover phase.
        return None;
    }
    // The chain id is only consulted when replay records are assembled; the
    // tip read below never assembles one. A fresh database reads as tip 0.
    let wal = BlockReplayStorage::new_without_genesis(
        &config
            .general_config
            .rocks_db_path
            .join(BLOCK_REPLAY_WAL_DB_NAME),
        config.genesis_config.chain_id.unwrap_or(0),
    );
    let tip = wal.latest_record_checked().unwrap_or(0);
    (tip < anchor).then_some(anchor)
}

/// Whether the configuration carries every genesis fact a main node needs —
/// the set an external node otherwise fetches from the main node at runtime.
fn genesis_facts_configured(genesis: &crate::config::GenesisConfig) -> bool {
    genesis.bridgehub_address.is_some()
        && genesis.bytecode_supplier_address.is_some()
        && genesis.chain_id.is_some()
        && genesis.genesis_input_path.is_some()
}

pub async fn run<State: ReadStateHistory + WriteState + StateInitializer + Clone>(
    runtime: &Runtime,
    config: Config,
) -> LaunchedNode {
    // A consensus start scheduled at a height this node has not reached yet:
    // `Some(anchor)` arms the pre-cutover mode — the node runs its
    // `general.node_role` behavior bounded to the anchor, then signals the
    // embedder to shut it down; the next start finds the write-ahead log
    // ending exactly at the anchor and runs consensus. `node_role` describes
    // only that pre-cutover behavior: from the anchor on, every consensus
    // node runs as a main node.
    let scheduled_cutover = scheduled_cutover_pending(&config);
    let node_role = if config.consensus_config.enabled && scheduled_cutover.is_none() {
        NodeRole::MainNode
    } else {
        config.general_config.node_role
    };
    if node_role != config.general_config.node_role {
        tracing::info!(
            configured_role = config.general_config.node_role.as_str(),
            "consensus governs this chain from its anchor; `general.node_role` \
             applies before the cutover only — running as a main node"
        );
    }

    let BoundListeners {
        rpc: rpc_listener,
        status: prebound_status_listener,
        prover_api: prebound_prover_api_listener,
    } = BoundListeners::bind_from_config(&config, node_role)
        .await
        .expect("failed to prebind node ports");
    report_static_config_metrics(&config);

    let role: &'static str = node_role.as_str();

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

    // One client for all main-node RPC; `None` on a main node (nothing to talk
    // to). Gated on the effective role: after a scheduled cutover the config
    // may still carry `main_node_rpc_url`, but a consensus node defers to no
    // other node.
    let main_node_client = if node_role.is_main() {
        None
    } else {
        config
            .general_config
            .main_node_rpc_url
            .as_ref()
            .map(|url| MainNodeClient::new(url).expect("failed to build main node RPC client"))
    };

    // Genesis facts come from the local configuration whenever it carries them
    // — a consensus configuration always does (validated) — and are fetched
    // from the main node otherwise. A consensus node must not depend on
    // another node's RPC to know its own chain, in either cutover phase.
    let (bridgehub_address, bytecode_supplier_address, chain_id, genesis_input_source) =
        if node_role.is_main() || genesis_facts_configured(&config.genesis_config) {
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
            let client = main_node_client
                .as_ref()
                .expect("Missing `main_node_rpc_url` in external node config");
            load_remote_config(client, &config.genesis_config)
                .await
                .expect("Cannot load remote config from Main Node")
        };

    // This is the only place where we initialize L1 provider, every component shares the same
    // cloned provider.
    let l1_provider = build_node_provider(
        &config.l1_provider_config,
        config.l1_watcher_config.poll_interval,
        config.l1_watcher_config.finalized_poll_interval,
        config.l1_watcher_config.logs_cache_capacity,
        ProviderKind::L1,
    )
    .await;
    // Genesis and the repository manager are initialized here (before the startup revert) so
    // that the `from_block_hash` guard can read the current local block hash.
    let diamond_proxy_l1 =
        L1State::resolve_diamond_proxy_l1(l1_provider.clone(), bridgehub_address, chain_id)
            .await
            .expect("failed to resolve L1 diamond proxy");

    let genesis = Genesis::new(
        genesis_input_source.clone(),
        diamond_proxy_l1.clone(),
        chain_id,
    );

    tracing::info!("Initializing RepositoryManager");
    let repositories = RepositoryManager::new(
        config.general_config.blocks_to_retain_in_memory,
        config.general_config.rocks_db_path.join(REPOSITORY_DB_NAME),
        &genesis,
    )
    .await;

    // Apply the `from_block_hash` idempotency guard once, up front, to derive the *effective*
    // rebuild config used for the rest of startup. If the configured rebuild already ran on a
    // prior startup (hash no longer matches), it is dropped here so BOTH the L1 revert (stage 1,
    // below) and the local block rebuild (stage 2, downstream) are skipped.
    let rebuild_config = if node_role.is_main() {
        let configured = config.sequencer_config.rebuild.clone();
        match configured.as_ref().and_then(|r| r.bounds()) {
            Some(bounds)
                if !from_block_hash_matches(
                    &repositories,
                    bounds.from_block_number,
                    bounds.from_block_hash,
                )
                .expect("failed to evaluate startup rebuild from_block_hash guard") =>
            {
                None
            }
            _ => configured,
        }
    } else {
        config.sequencer_config.rebuild.clone()
    };
    // What the local block-rebuild stage (stage 2) should replay, if anything.
    let rebuild_options = rebuild_config.as_ref().and_then(|r| r.rebuild_options());

    // Fetch the L1 state, performing the configured startup L1 revert first.
    tracing::info!("Reading L1 state");
    let l1_state = fetch_l1_state_with_startup_revert(
        &config,
        node_role,
        rebuild_config.as_ref(),
        &l1_provider,
        bridgehub_address,
        chain_id,
    )
    .await
    .expect("failed to determine L1 state");

    tracing::info!(?l1_state, "L1 state");
    l1_state.report_metrics();
    if node_role.is_main() {
        check_batch_verification_mismatch(
            &config.batch_verification_config,
            &l1_state.batch_verification,
        );
        check_required_operator_keys(&config);
    }

    // Effective pubdata mode used by all block-producing components.
    let effective_pubdata_mode: Option<PubdataMode> = if node_role.is_main() {
        Some(effective_main_node_pubdata_mode(&config))
    } else {
        // External nodes do not produce blocks; pubdata mode is irrelevant for them.
        None
    };
    if let (Some(pubdata_mode), true) = (effective_pubdata_mode, node_role.is_main()) {
        match (pubdata_mode, l1_state.da_input_mode) {
            (PubdataMode::Calldata | PubdataMode::Blobs, BatchDaInputMode::Validium)
            | (PubdataMode::Validium, BatchDaInputMode::Rollup) => {
                panic!(
                    "Pubdata mode doesn't correspond to pricing mode from the l1. \
                    L1 mode: {:?}, effective pubdata mode: {:?}",
                    l1_state.da_input_mode, pubdata_mode
                );
            }
            _ => {}
        }
    }

    warn_on_leftover_raft_storage(&config);

    tracing::info!("Initializing BlockReplayStorage");

    let (block_replay_storage, inserted_genesis_replay_record) = BlockReplayStorage::new(
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

    let (genesis_root_hash, genesis_root_leaves) = tree_db
        .root_info(0)
        .expect("Failed to get genesis root info")
        .expect("tree is not initialized");
    let tree_for_rpc = Arc::new(tree_db.clone());

    let committed_batch_provider = CommittedBatchProvider::new(
        runtime,
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

    let (
        last_l1_committed_block,
        last_l1_proved_block,
        last_l1_executed_block,
        last_l1_finalized_executed_block,
    ) = commit_proof_execute_block_numbers(&l1_state, &committed_batch_provider).await;

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

    // The disaster-fork backstop: a settler whose L1 has committed batches past
    // its local chain would faithfully recreate them from recovery — recreating
    // exactly the poisoned batches a fork just discarded. This state cannot
    // arise in normal operation (the batcher only commits blocks it has); it
    // means a truncated node restarted before the L1 revert step of the fork
    // runbook, so the guard names that step instead of letting the recovery
    // machinery work against the fork. Main nodes only: an external node never
    // commits batches and legitimately starts empty with L1 ahead, syncing the
    // gap from its peers.
    if node_role.is_main() && config.batcher_config.enabled {
        assert!(
            node_startup_state.last_l1_committed_block
                <= node_startup_state.block_replay_storage_last_block,
            "L1 has committed batches past this node's chain (last committed block on L1: \
             {}, local tip: {}). If this follows a disaster-fork truncation, the \
             committed-but-unexecuted batches above the fork point must be reverted on L1 \
             before the settler restarts — refusing to start rather than recreate and \
             re-commit discarded blocks",
            node_startup_state.last_l1_committed_block,
            node_startup_state.block_replay_storage_last_block,
        );
    }

    if let Some(from_block_number) = rebuild_options.as_ref().map(|o| o.from_block_number)
        && node_role.is_main()
    {
        // The assertion is only relevant for the main node.
        // External node can be started at any point and doesn't have to be in sync with L1.
        // But the main node is expected to only produce blocks on top of committed L1 blocks,
        // as those can't be re-sequenced.
        assert!(
            from_block_number > node_startup_state.last_l1_committed_block,
            "rebuild_from_block_number must be > last_l1_committed_block, got {} <= {}",
            from_block_number,
            node_startup_state.last_l1_committed_block
        );
    }

    // A truncated chain must not run consensus over engine state that recorded
    // progress past the cut — marshal would resume delivery above the new tip
    // and die on the delivery-order assert. The truncate tool flags exactly the
    // state it invalidated; the flag goes away with the runbook's clear step.
    // Checked before any service binds or spawns: the refusal must leave
    // nothing behind.
    if config.consensus_config.enabled {
        let engine_dir = config.general_config.rocks_db_path.join("consensus");
        if let Some(truncated_to) =
            consensus::read_truncation_flag(&engine_dir).unwrap_or_else(|err| {
                panic!(
                    "failed to read the truncation flag at {}: {err}",
                    engine_dir.display()
                )
            })
        {
            panic!(
                "this chain was truncated to block {truncated_to}, but the consensus \
                 engine state at {} predates the truncation and cannot be reused; \
                 clear the directory and restart (the fork runbook's \"clear the \
                 consensus engine state\" step)",
                engine_dir.display()
            );
        }
    }

    let finality_storage = Finality::new(FinalityStatus {
        last_committed_block: last_l1_committed_block,
        last_committed_batch: l1_state.last_committed_batch,
        last_executed_block: last_l1_executed_block,
        last_executed_batch: l1_state.last_executed_batch,
        last_finalized_executed_block: last_l1_finalized_executed_block,
        last_finalized_executed_batch: l1_state.last_finalized_executed_batch,
    });

    // `starting_block` - the first block to go through the pipeline. Invariant: a replay record for
    // this block must already exist. Note that this holds for `starting_block=0` as genesis is
    // always present in the system.
    let starting_block = if node_startup_state.l1_state.last_committed_batch > 0 {
        // todo: ideally this should be searched through p2p networking instead of RPC
        //       but too many things depend on this being initialized here right now
        //       once refactored we can get rid of `main_node_rpc_url` config param
        let last_matching_block = if let Some(client) = &main_node_client {
            find_last_matching_main_node_block(&repositories, client)
                .await
                .expect("Failed to find last matching block with main node")
        } else {
            node_startup_state.repositories_persisted_block
        };
        // Some batches committed - starting from an already committed batch
        determine_starting_block(
            &config,
            &node_startup_state,
            &state,
            last_matching_block,
            rebuild_options.as_ref(),
        )
    } else {
        // No batches committed - starting from genesis.
        0
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

    // MN sends `VerifyBatch` requests to the network and receives `PeerVerifyBatchResult`s back.
    let (verify_request_tx, verify_request_rx) = tokio::sync::mpsc::channel::<VerifyBatch>(16);
    let (verify_result_tx, verify_result_rx) =
        tokio::sync::mpsc::channel::<PeerVerifyBatchResult>(128);
    // `replay_*` carries replay records from the network service into the EN pipeline.
    let (replay_sender, replays_for_sequencer) = tokio::sync::mpsc::channel(128);
    // EN receives peer verification requests and broadcasts signed responses back to the network.
    let (verify_batch_tx, verify_batch_rx) = tokio::sync::mpsc::channel::<PeerVerifyBatch>(128);
    let (outgoing_verify_results, _) =
        tokio::sync::broadcast::channel::<PeerVerifyBatchResult>(128);

    // Single-sequencer mode: this node is always the leader and every block it produces
    // is immediately canonical. When BFT consensus is enabled, the pipeline is fed by
    // the consensus committed stream instead and these two seams are bypassed.
    let canonization_engine = NoopCanonization::new();
    let leadership = LeadershipSignal::AlwaysLeader;
    let network = if config.network_config.enabled {
        tracing::info!("initializing p2p networking");
        let batch_verification_policy_config: BatchVerificationPolicyConfig =
            config.batch_verification_config.clone().into();
        let (network_service, bound_network_ports) = if node_role.is_main() {
            let (_, accepted_verifier_signers) =
                effective_verification_policy(&batch_verification_policy_config, &l1_state);
            NetworkService::new(
                config.network_config.clone().into(),
                runtime.clone(),
                ZksProtocolConfig::MainNode(MainNodeProtocolConfig {
                    accepted_verifier_signers,
                    verify_result_tx: verify_result_tx.clone(),
                    // A committee validator also verifies its peers' batches:
                    // the same session that would serve an EN carries the
                    // verifier handshake and request/response traffic.
                    verification: config.batch_verification_config.client_enabled.then(|| {
                        ExternalNodeVerifierConfig {
                            signing_key: config.batch_verification_config.signing_key.clone(),
                            verify_batch_tx: verify_batch_tx.clone(),
                            outgoing_verify_results: outgoing_verify_results.clone(),
                        }
                    }),
                }),
                block_replay_storage.clone(),
                zk_provider_factory,
            )
            .await
        } else {
            let record_overrides = config
                .sequencer_config
                .en_replay_record_overrides
                .iter()
                .map(|(block_number, db_key)| RecordOverride {
                    block_number: *block_number,
                    db_key: db_key.clone(),
                })
                .collect();
            NetworkService::new(
                config.network_config.clone().into(),
                runtime.clone(),
                ZksProtocolConfig::ExternalNode(ExternalNodeProtocolConfig {
                    starting_block: Arc::new(RwLock::new(starting_block)),
                    record_overrides,
                    max_blocks_per_message: config
                        .sequencer_config
                        .en_max_blocks_per_replay_message,
                    replay_sender,
                    verification: config.batch_verification_config.client_enabled.then(|| {
                        ExternalNodeVerifierConfig {
                            signing_key: config.batch_verification_config.signing_key.clone(),
                            verify_batch_tx: verify_batch_tx.clone(),
                            outgoing_verify_results: outgoing_verify_results.clone(),
                        }
                    }),
                }),
                block_replay_storage.clone(),
                zk_provider_factory,
            )
            .await
        }
        .expect("failed to create network service");
        network_service.spawn(runtime, node_role.is_main().then_some(verify_request_rx));
        Some(bound_network_ports)
    } else if node_role.is_main() {
        tracing::info!(
            "p2p networking is disabled; to enable set `network.enabled=true` and populate `network.secret_key`"
        );
        None
    } else {
        panic!(
            "EN cannot run without p2p networking; to fix: \
            set `network.enabled=true` to enable p2p networking, \
            populate `network.secret_key` with a 256-bit ECDSA key (can be randomly generated locally), \
            populate `network.boot_nodes` with at least one known node from the chain. \
            See https://github.com/matter-labs/zksync-os-server/pull/873 for full rollout instructions."
        );
    };

    // Channel from L1Sender<CommitCommand> to L1CommitWatcher.
    // Initialized to startup's last_committed_batch so any commit above that value
    // which the pipeline didn't submit in this session triggers a restart.
    let (commit_submitted_tx, commit_submitted_rx) =
        watch::channel(node_startup_state.l1_state.last_committed_batch);

    tracing::info!("Initializing L1 Watchers");
    runtime.spawn_critical_task(
        "l1 commit watcher",
        L1CommitWatcher::create_watcher(
            config.l1_watcher_config.clone().into(),
            node_startup_state.l1_state.diamond_proxy_l1.clone(),
            committed_batch_provider.clone(),
            finality_storage.clone(),
            l1_state.l1_block_number,
            // Only nodes that actually submit commit txs locally should arm the
            // `UnexpectedCommit` guard — a main node running with
            // `batcher_config.enabled = false` must not panic when a commit submitted
            // by another node lands on L1.
            (node_role.is_main() && config.batcher_config.enabled).then_some(commit_submitted_rx),
        )
        .await
        .expect("failed to start L1 commit watcher")
        .run(()),
    );

    runtime.spawn_critical_task(
        "l1 execute watcher",
        L1ExecuteWatcher::create_watcher(
            config.l1_watcher_config.clone().into(),
            node_startup_state.l1_state.diamond_proxy_l1.clone(),
            committed_batch_provider.clone(),
            finality_storage.clone(),
        )
        .await
        .expect("failed to start L1 execute watcher")
        .run(()),
    );

    runtime.spawn_critical_task(
        "l1 finalized execute watcher",
        L1FinalizedExecuteWatcher::create_finalized_watcher(
            config.l1_watcher_config.clone().into(),
            node_startup_state.l1_state.diamond_proxy_l1.clone(),
            committed_batch_provider.clone(),
            finality_storage.clone(),
        )
        .await
        .expect("failed to start finalized L1 execute watcher")
        .run(()),
    );

    // External nodes restart themselves on an L1 batch revert to resync correct data.
    if node_role.is_external() {
        runtime.spawn_critical_task(
            "l1 revert watcher",
            L1RevertWatcher::create_watcher(
                config.l1_watcher_config.clone().into(),
                node_startup_state.l1_state.diamond_proxy_l1.clone(),
                node_startup_state.l1_state.l1_block_number,
            )
            .run(),
        );
    }

    let first_replay_record = block_replay_storage.get_replay_record(starting_block);
    assert!(
        first_replay_record.is_some() || starting_block == 1,
        "Unless it's a new chain, replay record must exist"
    );

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

    if config
        .sequencer_config
        .tx_validator
        .policy_service
        .url
        .is_some()
    {
        let exec_version = ExecutionVersion::try_from(current_protocol_version)
            .expect("Cannot determine execution version");
        assert!(
            exec_version >= ExecutionVersion::V6,
            "Policy service requires execution version V6 or later (protocol >= v31.0), \
             but current protocol version {current_protocol_version} uses {exec_version:?}"
        );
    }

    // Transaction acceptance state - tracks whether we're accepting new transactions
    // Main nodes: accepts, but may switch to reject when `sequencer_max_blocks_to_produce` blocks are produced
    // External nodes: always accepts, but may be rejected on the main node side during forwarding
    let (tx_acceptance_state_sender, tx_acceptance_state_receiver) =
        watch::channel(TransactionAcceptanceState::Accepting);

    let (stop_sender, stop_receiver) = watch::channel(false);
    let stop_sender_for_shutdown = stop_sender.clone();
    runtime.spawn_with_graceful_shutdown_signal(|shutdown| async move {
        let _guard = shutdown.await;
        let _ = stop_sender_for_shutdown.send(true);
    });

    let tx_forwarder = if config.consensus_config.enabled
        && config.consensus_config.role.is_observer()
        && scheduled_cutover.is_none()
    {
        // A consensus observer includes nothing itself: its RPC keeps a local
        // mirror (pending views stay coherent) and forwards to the validators.
        Some(build_round_robin_tx_forwarder(&config.consensus_config.tx_forward_rpc_urls).await)
    } else if !node_role.is_main()
        && let Some(url) = config.general_config.main_node_rpc_url.as_ref()
    {
        // Following (an external node, or any pre-cutover follower): forward to
        // the node that sequences today.
        Some(build_static_tx_forwarder(url).await)
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
        let pubdata_mode =
            effective_pubdata_mode.expect("effective_pubdata_mode is always Some on the Main Node");
        let gas_adjuster_config = gas_adjuster_config(
            config.gas_adjuster_config.clone(),
            pubdata_mode,
            config.l1_sender_config.max_priority_fee_per_gas.0,
        );
        let gas_adjuster = GasAdjuster::new(
            l1_provider.clone().erased(),
            gas_adjuster_config,
            pubdata_price_sender,
            blob_fill_ratio_sender,
            sidecar_receiver,
        )
        .await
        .unwrap();
        runtime.spawn_critical_task("gas adjuster", gas_adjuster.run());
    }

    // ========== Start BlockContextProvider and its state ===========
    tracing::info!("Initializing BlockContextProvider");

    // The base token price updater owns the price channel and hands back a cloneable handle.
    // External nodes don't run the updater, so they get a `pending` handle whose value stays unset.
    let base_token_price_handle = if node_role.is_main() {
        let external_price_api_client_config = config
            .external_price_api_client_config
            .clone()
            .expect("external_price_api_client config must be set for Main Node");
        let (base_token_price_updater, base_token_price_handle) = BaseTokenPriceUpdater::new(
            &l1_state,
            base_token_price_updater_config(
                &config.base_token_price_updater_config,
                &config.l1_sender_config,
            ),
            external_price_api_client_config.into(),
        )
        .await
        .expect("Failed to initialize BaseTokenPriceUpdater");
        runtime.spawn_critical_task("base token price updater", base_token_price_updater.run());
        base_token_price_handle
    } else {
        BaseTokenPriceHandle::pending()
    };

    // todo: `BlockContextProvider` initialization and its dependencies
    // should be moved to `sequencer`
    let fee_provider = FeeProvider::new(
        config.fee_config.clone().into(),
        pubdata_price_receiver,
        blob_fill_ratio_receiver,
        base_token_price_handle.clone(),
        effective_pubdata_mode,
    );

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

    let pool = Pool::new(
        runtime.clone(),
        genesis.clone(),
        &node_startup_state.l1_state,
        zksync_os_mempool::Config {
            node_role,
            chain_id,
            interop_roots_per_tx: config.sequencer_config.interop_roots_per_tx,
            bytecode_supplier_address,
            l1_watcher_config: {
                let mut watcher_config: zksync_os_l1_watcher::L1WatcherConfig =
                    config.l1_watcher_config.clone().into();
                // Under consensus, deposits and protocol upgrades are ingested at
                // the *finalized* L1 boundary rather than `confirmations` blocks
                // behind the tip: they become block content that every validator
                // verifies against its own L1 view before voting, and a finalized
                // block is irrevocable — the deep-reorg remedy a single sequencer
                // had (roll the chain back and re-sequence) does not exist here.
                // The cost is deposit latency (L1 finality, ~13 min on Ethereum);
                // the alternative is a finalized L2 block referencing an L1 event
                // that no longer exists.
                watcher_config.finalized_ingestion = config.consensus_config.enabled;
                watcher_config
            },
            interop_fee_updater_config: config.interop_fee_updater_config.clone().into(),
        },
        // todo: eventually this should be initialized inside `Pool::new`
        l2_subpool.clone(),
    )
    .await
    .expect("failed to create mempool");
    // Durability watermark: the applier reports persisted block numbers; consumed by the
    // block executor (pacing workaround) or by the consensus environment (commit acks).
    let (applied_block_number_sender, applied_block_number_receiver) = watch::channel(None);

    // Rollback guard (decision unit-tested in the consensus module): running
    // single-sequencer on a chain that has consensus state requires an explicit
    // acknowledgment. Nothing is ever deleted.
    if !config.consensus_config.enabled {
        let engine_dir = config.general_config.rocks_db_path.join("consensus");
        let has_consensus_state =
            !consensus_engine_state_is_fresh(&engine_dir).unwrap_or_else(|err| {
                panic!(
                    "failed to inspect consensus engine state at {}: {err}",
                    engine_dir.display()
                )
            });
        consensus::check_rollback_acknowledged(
            has_consensus_state,
            config.consensus_config.acknowledge_rollback,
        )
        .unwrap_or_else(|err| panic!("{err} (consensus engine state: {})", engine_dir.display()));
    }

    // In consensus mode the mempool and fee sourcing drive the consensus block builder;
    // otherwise they drive the local block production loop.
    // TODO(consensus): with no local production loop there is no "block under
    // construction", so RPC surfaces that peek at it (pending-block context for eth_call
    // and gas estimation) fall back to the latest committed block in consensus mode.
    let (block_context_provider, consensus_committed_receiver, consensus_status_source) = if config
        .consensus_config
        .enabled
        && scheduled_cutover.is_none()
    {
        // The mempool expects to be initialized with the last replayed block exactly
        // once; the local production pipeline does this on its first replay, consensus
        // mode does it here.
        let mut pool = pool;
        let wal_tip_record = block_replay_storage
            .get_replay_record(block_replay_storage.latest_record())
            .expect("write-ahead log must contain its latest record");
        // Feed the watchers as-of-*after* the tip: canonical draining resumes at
        // tip+1 (redeliveries at or below the tip are absorbed without touching
        // the pool), so seeding at the tip's *starting* cursors would re-queue
        // the tip block's already-committed L1 inputs — stale entries at the
        // queue front that the l1 subpool's drain-order assert then trips over
        // at the first post-restart priority op.
        pool.init(
            &wal_tip_record,
            zksync_os_consensus_execution::builder::derive_next_cursors(&wal_tip_record),
        )
        .await;
        // The verification-side view of locally-watched L1 inputs, taken before the
        // pool moves into the builder.
        let l1_inputs_view = pool.l1_inputs_view();

        // The operational timing this configuration implies, in one line an
        // operator can sanity-check (the derivations are documented in the
        // "Operating a committee" chapter).
        {
            let block_time = config.sequencer_config.block_time.as_secs_f64();
            let epoch = std::time::Duration::from_secs_f64(
                block_time * config.consensus_config.epoch_length as f64,
            );
            let retention = config.consensus_config.epoch_retention.max(1);
            let catch_up =
                std::time::Duration::from_secs_f64(epoch.as_secs_f64() * retention as f64);
            tracing::info!(
                epoch_under_load = ?epoch,
                emergency_rotation_sprint_bound = ?epoch,
                catch_up_window_under_load = ?catch_up,
                idle_heartbeat = ?config.consensus_config.idle_heartbeat,
                "consensus timing characteristics"
            );
        }
        // The idle policy is the deliberate strategy for quiet chains — see the
        // `idle_policy` module for the full story. Sprint targets come from the
        // schedule: an entry still waiting for its boundary keeps idle leaders
        // producing so it activates without traffic.
        let idle_policy = if config.consensus_config.idle_heartbeat.is_zero() {
            zksync_os_consensus_execution::idle_policy::IdlePolicy::legacy()
        } else {
            zksync_os_consensus_execution::idle_policy::IdlePolicy::heartbeat(
                config.consensus_config.idle_heartbeat,
                std::num::NonZeroU64::new(config.consensus_config.epoch_length)
                    .expect("validated: epoch_length is nonzero"),
                config
                    .consensus_config
                    .committees
                    .iter()
                    .map(|entry| entry.activation_epoch)
                    .collect(),
            )
        };
        let builder = zksync_os_consensus_execution::ConsensusBlockBuilder::new(
            pool,
            fee_provider,
            zksync_os_consensus_execution::BuilderConfig {
                l2_chain_id: chain_id,
                // With Gateway settlement removed, the settlement layer is always L1
                // (mirrors the sequencer's `set_sl_chain_id(l1_chain_id)`).
                sl_chain_id: node_startup_state.l1_state.l1_chain_id,
                gas_limit: config.sequencer_config.block_gas_limit,
                pubdata_limit: config.sequencer_config.block_pubdata_limit_bytes,
                fee_collector_address: config.sequencer_config.fee_collector_address,
                block_time: config.sequencer_config.block_time,
                // Idle chains keep producing (empty) blocks at the same cadence for now;
                // whether to slow the idle cadence is a tuning decision for later.
                idle_block_deadline: config.sequencer_config.block_time,
                max_transactions_in_block: config.sequencer_config.max_transactions_in_block,
                interop_roots_per_block: config.sequencer_config.interop_roots_per_block,
                // Same anchor the ChainAnchor below is resolved from: epochs count
                // from the consensus genesis, not from chain height 0.
                era_anchor: config.consensus_config.genesis_height,
            },
            idle_policy,
        );

        // The consensus anchor: the block the consensus genesis stands for — the
        // chain's real genesis on a fresh chain (height 0), the agreed cutover block
        // on a chain migrated from single-sequencer operation. Everything the first
        // consensus block is verified against comes from the anchored block's own
        // record.
        let anchor_height = config.consensus_config.genesis_height;
        let anchor_record = block_replay_storage
            .get_replay_record(anchor_height)
            .unwrap_or_else(|| {
                panic!(
                    "the write-ahead log has no record at the consensus genesis height                      {anchor_height} — this node is missing history up to the agreed anchor"
                )
            });
        let genesis_state = genesis.state().await;
        let anchor_el_hash = if anchor_height == 0 {
            genesis_state.header.hash()
        } else {
            repositories
                .get_block_by_number(anchor_height)
                .ok()
                .flatten()
                .unwrap_or_else(|| {
                    panic!(
                        "repositories have no block at the consensus genesis height                          {anchor_height} — this node is missing history up to the agreed anchor"
                    )
                })
                .hash()
        };
        let anchor =
            zksync_os_consensus_execution::ChainAnchor::from_record(&anchor_record, anchor_el_hash);
        // Resolve the actual consensus era once from this node's local anchor.
        // The startup guard, durable marker, and diagnostic fingerprint must all
        // refer to this exact digest rather than independently re-deriving it.
        let consensus_era = {
            use commonware_cryptography::Digestible as _;
            let digest = zksync_os_wire::ConsensusBlock::genesis_at(
                anchor.genesis_height,
                anchor.genesis_block_hash,
            )
            .digest();
            <[u8; 32]>::try_from(digest.as_ref()).expect("32-byte digest")
        };
        let committed_height = block_replay_storage.latest_record();
        // The tip's hash comes from the write-ahead log — the same store the
        // committed height comes from, written synchronously ahead of the
        // repositories. Reading the (asynchronously persisted) repositories
        // here can yield nothing right after an unclean restart, and a leader
        // without its committed hash passes every turn: proposing needs the
        // parent hash, and it refreshes only on the next commit — which after
        // a committee-wide restart in that state would never come.
        let committed_el_hash = Some(block_replay_storage.canonical_block_hash(committed_height));

        let validation = zksync_os_consensus_execution::ProposalValidation {
            config: std::sync::Arc::new(zksync_os_consensus_execution::ValidityConfig {
                max_timestamp_skew: config.consensus_config.max_timestamp_skew,
                chain_id,
                // Same source as the builder's `sl_chain_id` above.
                sl_chain_id: node_startup_state.l1_state.l1_chain_id,
                fee_collector_address: config.sequencer_config.fee_collector_address,
                gas_limit: config.sequencer_config.block_gas_limit,
                pubdata_limit: config.sequencer_config.block_pubdata_limit_bytes,
                max_transactions: config.sequencer_config.max_transactions_in_block,
                max_encoded_record_size: config.consensus_config.max_message_size.get() as usize,
                fee: config.fee_config.clone().into(),
            }),
            inputs: std::sync::Arc::new(l1_inputs_view),
        };

        // The node's own certificate store: the consensus engine's archives are a
        // rebuildable cache, this is the durable record (fed by the activity
        // observer and the commit path; surfaced in /status as the certified
        // watermark).
        let finality_store = std::sync::Arc::new(
            zksync_os_consensus_execution::FinalityStore::open(
                &config.general_config.rocks_db_path.join("finality"),
            )
            .expect("failed to open the finality store"),
        );

        // The consensus-era guards (the decision matrix is pure and unit-tested in
        // the consensus module; this block only gathers its inputs and applies the
        // outcome).
        {
            let engine_dir = config.general_config.rocks_db_path.join("consensus");
            let engine_state_is_fresh = consensus_engine_state_is_fresh(&engine_dir)
                .unwrap_or_else(|err| {
                    panic!(
                        "failed to inspect consensus engine state at {}: {err}",
                        engine_dir.display()
                    )
                });
            let recorded = finality_store
                .consensus_era()
                .expect("failed to read the consensus era");
            let acknowledged_fork =
                consensus::parse_acknowledge_fork(&config.consensus_config.acknowledge_fork)
                    .expect("invalid `consensus.acknowledge_fork`");
            let decision = consensus::decide_consensus_era(
                recorded,
                consensus_era,
                engine_state_is_fresh,
                committed_height,
                anchor.genesis_height,
                acknowledged_fork,
                anchor.genesis_block_hash,
            )
            .unwrap_or_else(|err| {
                panic!("{err} (consensus engine state: {})", engine_dir.display())
            });
            match decision {
                consensus::EraDecision::Proceed => {}
                consensus::EraDecision::Adopt => {
                    if recorded.is_some() {
                        tracing::warn!(
                            anchor_height = anchor.genesis_height,
                            "consensus era changed over cleared engine state — starting a new \
                             consensus era at the configured anchor"
                        );
                    }
                    finality_store
                        .record_consensus_era(consensus_era, anchor.genesis_height)
                        .expect("failed to record the consensus era");
                }
            }
        }

        let (committed_payload_sender, committed_payload_receiver) = tokio::sync::mpsc::channel(1);
        let env = zksync_os_consensus_execution::NodeExecutionEnv::new(
            state.clone(),
            anchor,
            committed_height,
            committed_el_hash,
            committed_payload_sender,
            applied_block_number_receiver.clone(),
            config.sequencer_config.interop_roots_per_block,
        )
        .with_builder(std::sync::Arc::new(tokio::sync::Mutex::new(builder)))
        .with_validity(validation)
        .with_finality_store(finality_store.clone());

        let setup = consensus::ConsensusSetup::from_config(
            &config.consensus_config,
            config.general_config.rocks_db_path.join("consensus"),
            chain_id,
        )
        .expect("invalid consensus configuration");

        // Progress and metrics surfaces for `/status`, fed from inside the consensus
        // world.
        let (finalized_sender, finalized_receiver) = tokio::sync::watch::channel(None);
        let (metrics_encoder_sender, metrics_encoder_receiver) = tokio::sync::watch::channel(None);
        let (registry_status_sender, registry_status_receiver) = tokio::sync::watch::channel(None);
        let chain_fingerprint =
            chain_fingerprint::chain_fingerprint(&config, consensus_era, &setup.observers);
        tracing::info!(
            chain_fingerprint,
            "committee-uniform configuration and consensus-era fingerprint"
        );
        match &setup.registry {
            Some(registry) => tracing::info!(
                mode = %registry.mode,
                address = ?registry.address,
                flip_epoch = registry.flip_epoch,
                "on-chain validator registry"
            ),
            // A derivation trail with the registry disabled means this chain ran
            // a registry mode before — a deliberate recovery/rollback, or a mode
            // misconfiguration. Either way the operator should know config is
            // governing while registry records exist.
            None => {
                let recorded_derivations = match finality_store.registry_derivations() {
                    Ok(derivations) => derivations.len(),
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            "registry derivation records are unreadable, but \
                             `consensus.registry_mode` is `schedule`: ignoring the \
                             non-governing trail"
                        );
                        0
                    }
                };
                if recorded_derivations > 0 {
                    tracing::warn!(
                        recorded_derivations,
                        "registry derivation records exist but \
                         `consensus.registry_mode` is `schedule`: the config \
                         schedule governs (rollback/recovery state) — switch back \
                         to a registry mode once the registry is usable again"
                    );
                }
            }
        }
        let status_source = zksync_os_status_server::ConsensusStatusSource {
            role: setup.role.as_str(),
            committee_size: setup.committee.len(),
            validator: {
                use commonware_codec::Encode as _;
                use commonware_cryptography::Signer as _;
                alloy::hex::encode(setup.network_key.public_key().encode())
            },
            finalized: finalized_receiver,
            applied_height: applied_block_number_receiver.clone(),
            finality_certified: finality_store.watermark_subscription(),
            chain_fingerprint,
            registry: registry_status_receiver,
            metrics_encoder: metrics_encoder_receiver,
        };

        let (consensus_shutdown_sender, consensus_shutdown) = tokio::sync::oneshot::channel();
        let (_thread, consensus_dead) = consensus::spawn(
            setup,
            env,
            l2_subpool.clone(),
            consensus::ConsensusObservability {
                finalized: finalized_sender,
                metrics_encoder: metrics_encoder_sender,
                finality: finality_store,
                registry: registry_status_sender,
            },
            consensus_shutdown,
        );
        runtime.spawn_critical_task("consensus watchdog", async move {
            // Held for the watchdog's lifetime: when the node runtime tears this task
            // down (shutdown), the dropped sender tells consensus to stop gracefully.
            let _shutdown_consensus_when_dropped = consensus_shutdown_sender;
            let _ = consensus_dead.await;
            panic!("consensus stack died; the node cannot continue without it");
        });

        (None, Some(committed_payload_receiver), Some(status_source))
    } else {
        let provider = BlockContextProvider::new(
            fee_provider,
            pool,
            zksync_os_sequencer::execution::block_context_provider::Config {
                l2_chain_id: chain_id,
                l1_chain_id: node_startup_state.l1_state.l1_chain_id,
                gas_limit: config.sequencer_config.block_gas_limit,
                pubdata_limit: config.sequencer_config.block_pubdata_limit_bytes,
                fee_collector_address: config.sequencer_config.fee_collector_address,
                block_time: config.sequencer_config.block_time,
                service_block_delay: config.sequencer_config.service_block_delay,
                max_transactions_in_block: config.sequencer_config.max_transactions_in_block,
                // We set the value to the same as for the batch, since it should be enforced by batcher, but don't want to exceed it for the block
                interop_roots_per_block: config.batcher_config.interop_roots_per_batch_limit,
                // A scheduled cutover ends local production exactly at the
                // consensus anchor; the sentinel below handles the rest.
                produce_up_to_block: scheduled_cutover,
            },
            last_constructed_block_ctx_sender,
        );
        (Some(provider), None, None)
    };

    // The cutover sentinel: on a boot that precedes the scheduled consensus
    // anchor, watch the write-ahead log and tell the embedder the moment it
    // reaches the anchor. The bounded sources above guarantee nothing is
    // written past the anchor; the embedder shuts the node down, and the next
    // start finds the log ending exactly there and runs consensus.
    let (scheduled_cutover_reached, scheduled_cutover_status) = match scheduled_cutover {
        Some(anchor) => {
            let tip = block_replay_storage.latest_record();
            tracing::info!(
                anchor,
                tip,
                pre_cutover_role = config.general_config.node_role.as_str(),
                "scheduled consensus cutover armed; running the pre-cutover role \
                 until the chain reaches the anchor"
            );
            let (reached_sender, reached_receiver) = watch::channel(false);
            let (tip_sender, tip_receiver) = watch::channel(tip);
            let wal = block_replay_storage.clone();
            runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
                let mut poll = tokio::time::interval(std::time::Duration::from_millis(250));
                tokio::pin!(shutdown);
                loop {
                    tokio::select! {
                        _ = &mut shutdown => return,
                        _ = poll.tick() => {}
                    }
                    let tip = wal.latest_record();
                    let _ = tip_sender.send(tip);
                    if tip >= anchor {
                        tracing::info!(
                            anchor,
                            "the chain reached the scheduled consensus anchor; the \
                             next start of this node runs consensus from here"
                        );
                        let _ = reached_sender.send(true);
                        return;
                    }
                }
            });
            (
                Some(reached_receiver),
                Some(zksync_os_status_server::ScheduledCutoverStatusSource {
                    genesis_height: anchor,
                    tip: tip_receiver,
                }),
            )
        }
        None => (None, None),
    };

    // ========== Start L1 Persist Batch Watcher ===========

    runtime.spawn_critical_task("l1 batch persist watcher", {
        let config = config.l1_watcher_config.clone();
        let diamond_proxy_l1 = node_startup_state.l1_state.diamond_proxy_l1.clone();
        let persistent_batch_storage = persistent_batch_storage.clone();
        async move {
            L1PersistBatchWatcher::create_watcher(
                config.into(),
                diamond_proxy_l1,
                persistent_batch_storage,
            )
            .run(())
            .await
        }
    });

    // ========== Start Sequencer ===========
    let repositories_clone = repositories.clone();
    runtime.spawn_critical_task(
        "repository persist loop",
        repositories_clone.run_persist_loop(),
    );
    let state_clone = state.clone();
    runtime.spawn_critical_task(
        "state compact loop",
        state_clone.compact_periodically_optional(),
    );

    let replay_archive =
        init_replay_archive(config.replay_archive_config.clone().into(), runtime).await;
    if let (Some((replay_archive_sender, _)), Some(inserted_genesis_replay_record)) =
        (&replay_archive, inserted_genesis_replay_record)
    {
        let (genesis_replay_record, genesis_hash) = inserted_genesis_replay_record.split();
        replay_archive_sender
            .send((genesis_hash, genesis_replay_record))
            .await
            .expect("replay archive component stopped before accepting genesis replay record");
    }
    let (replay_archive_sender, replay_archiver) = replay_archive.unzip();
    let archiving_block_replay_storage =
        ReplayArchivingWriteReplay::new(block_replay_storage, replay_archive_sender);

    let PipelineHandles {
        backpressure_acceptance_rx,
        pipeline_snapshot_rx,
        prover_api_port,
    } = if node_role.is_main() {
        run_main_node_pipeline(
            &config,
            l1_provider.clone(),
            node_startup_state,
            archiving_block_replay_storage,
            runtime,
            state.clone(),
            starting_block,
            rebuild_options,
            repositories.clone(),
            block_context_provider,
            consensus_committed_receiver,
            (applied_block_number_sender, applied_block_number_receiver),
            tree_db,
            finality_storage.clone(),
            chain_id,
            tx_acceptance_state_sender,
            sidecar_sender,
            committed_batch_provider.clone(),
            canonization_engine,
            leadership,
            stop_receiver.clone(),
            commit_submitted_tx,
            verify_request_tx,
            verify_result_rx,
            verify_batch_rx,
            outgoing_verify_results.clone(),
            effective_pubdata_mode.expect("effective_pubdata_mode is always Some on the Main Node"),
            replay_archiver,
            prebound_prover_api_listener,
        )
        .await
    } else {
        run_en_pipeline(
            &config,
            replays_for_sequencer,
            committed_batch_provider.clone(),
            node_startup_state,
            archiving_block_replay_storage,
            runtime,
            block_context_provider
                .expect("external nodes always run the local block context provider"),
            state.clone(),
            tree_db,
            repositories.clone(),
            finality_storage.clone(),
            stop_receiver.clone(),
            tx_acceptance_state_sender,
            chain_id,
            verify_batch_rx,
            outgoing_verify_results.clone(),
            scheduled_cutover,
        )
        .await
    };

    // Aggregate all "not accepting" signals into a single combined receiver for the RPC server.
    // Register additional sources here as needed — no other logic changes required.
    let combined_acceptance_rx = {
        let (mut gate, rx) = acceptance::TxAcceptanceGate::new();
        gate.register(tx_acceptance_state_receiver); // BlockProductionDisabled
        gate.register(backpressure_acceptance_rx); // PipelineBackpressure
        runtime.spawn_critical_task("tx acceptance gate", gate.run(stop_receiver.clone()));
        rx
    };

    let rpc_ready: Arc<OnceLock<()>> = Arc::new(OnceLock::new());

    // ======== Start Status Server ========
    let status_port = if config.status_server_config.enabled {
        let status_listener = prebound_status_listener
            .expect("status_server is enabled but status listener was not prebound");
        let port = status_listener
            .local_addr()
            .expect("status server local_addr")
            .port();
        let status_state = StatusServerState {
            pipeline_snapshot: pipeline_snapshot_rx,
            consensus: Arc::new(consensus_status_source),
            scheduled_cutover: scheduled_cutover_status,
            ready: rpc_ready.clone(),
        };
        runtime.spawn_critical_with_graceful_shutdown_signal(
            "status server",
            |shutdown| async move {
                run_status_server(status_listener, shutdown, status_state)
                    .await
                    .expect("failed to run status server");
            },
        );
        Some(port)
    } else {
        None
    };

    // =========== Start JSON RPC ========
    let rpc_port = rpc_listener
        .local_addr()
        .expect("rpc server local_addr")
        .port();

    let repositories_for_wait = repositories.clone();
    let wait_for_db = async move {
        // Wait for repositories to be ready to be used in RPC.
        repositories_for_wait
            .wait_for_db_ready_to_process_blocks()
            .await;
        // `rpc::spawn` awaits this future before serving.
        let _ = rpc_ready.set(());
    };
    let rpc_policy_client = config
        .sequencer_config
        .tx_validator
        .policy_service
        .build_client(zksync_os_tx_validators::policy_client::Component::Rpc);
    zksync_os_rpc::spawn(
        config.rpc_config.into(),
        rpc_listener,
        chain_id,
        bridgehub_address,
        bytecode_supplier_address,
        rpc_storage,
        l2_subpool,
        genesis_input_source,
        combined_acceptance_rx,
        last_constructed_block_ctx_receiver,
        tx_forwarder,
        rpc_policy_client,
        runtime,
        wait_for_db,
    )
    .await
    .expect("failed to spawn rpc server");
    let startup_time = process_started_at.elapsed();
    GENERAL_METRICS.startup_time[&"total"].set(startup_time.as_secs_f64());
    tracing::info!("All components scheduled for initialization in {startup_time:?}");

    LaunchedNode {
        ports: ServerPorts {
            rpc: rpc_port,
            status: status_port,
            prover_api: prover_api_port,
            network,
        },
        scheduled_cutover_reached,
    }
}

/// Checks whether block `rebuild.from_block_number` currently has the expected `rebuild.from_block_hash`.
///
/// Returns `Ok(false)` (operation should be skipped) when the hashes differ — distinguishing the
/// two reasons in the logs:
/// - block missing locally: likely a misconfigured `from_block_number` (typo / beyond local tip);
/// - hash changed: the rebuild/revert already ran on a previous startup (the expected case).
fn from_block_hash_matches(
    repositories: &dyn ReadRepository,
    from_block_number: u64,
    from_block_hash: alloy::primitives::BlockHash,
) -> anyhow::Result<bool> {
    let current_hash = repositories
        .get_block_by_number(from_block_number)
        .with_context(|| format!("failed to read block {from_block_number} from local repository"))?
        .map(|b| b.hash());
    Ok(match current_hash {
        Some(hash) if hash == from_block_hash => true,
        Some(hash) => {
            tracing::info!(
                from_block_number,
                current_hash = ?hash,
                ?from_block_hash,
                "skipping startup rebuild/revert: from_block_hash changed (already ran)"
            );
            false
        }
        None => {
            tracing::warn!(
                from_block_number,
                ?from_block_hash,
                "skipping startup rebuild/revert: block `from_block_number` not found locally \
                 (check `from_block_number` is correct — it may be a typo or beyond the local tip)"
            );
            false
        }
    })
}

/// Fetches the L1 state, performing any configured startup L1 revert first, and returns the
/// post-revert state.
async fn fetch_l1_state_with_startup_revert(
    config: &Config,
    node_role: NodeRole,
    rebuild: Option<&RebuildConfig>,
    l1_provider: &NodeProvider,
    bridgehub_address: Address,
    chain_id: u64,
) -> anyhow::Result<L1State> {
    // The batcher node must wait for any pending L1 commit/prove/execute transactions (from a
    // prior run) to be mined before starting, so it doesn't conflict with itself. Non-batcher
    // nodes never submit L1 transactions, so they don't need this wait: calling
    // fetch_finalized on them would spuriously fail when a concurrently running batcher node keeps
    // submitting new batch transactions.
    let use_finalized = node_role.is_main() && config.batcher_config.enabled;
    let l1_state = L1State::fetch_with_finality(
        use_finalized,
        l1_provider.clone(),
        bridgehub_address,
        chain_id,
    )
    .await
    .context("failed to fetch L1 state")?;

    if node_role.is_main()
        && let Some(rebuild) = rebuild
    {
        let l1_revert_ran = revert_l1_on_startup(rebuild, config, &l1_state, l1_provider)
            .await
            .context("startup l1 revert failed")?;

        if l1_revert_ran {
            // The revert invalidated the batch-finality numbers; re-fetch so the returned state
            // reflects the post-revert chain.
            return L1State::fetch_with_finality(
                use_finalized,
                l1_provider.clone(),
                bridgehub_address,
                chain_id,
            )
            .await
            .context("failed to fetch L1 state after startup revert");
        }
    }

    Ok(l1_state)
}

/// Handles the caller wires into other subsystems after launching the pipeline.
struct PipelineHandles {
    /// Registered into the `TxAcceptanceGate`.
    backpressure_acceptance_rx: watch::Receiver<TransactionAcceptanceState>,
    /// Per-component pipeline state, exposed via the status server's `/status/pipeline`.
    pipeline_snapshot_rx: watch::Receiver<PipelineSnapshot>,
    /// Prover API port, reported by the status server. `None` on external nodes.
    prover_api_port: Option<u16>,
}

#[allow(clippy::too_many_arguments)]
async fn run_main_node_pipeline(
    config: &Config,
    l1_provider: NodeProvider,
    node_state_on_startup: NodeStateOnStartup,
    block_replay_storage: impl WriteReplay + Clone,
    runtime: &Runtime,
    state: impl ReadStateHistory + WriteState + Clone,
    starting_block: u64,
    rebuild_options: Option<RebuildOptions>,
    repositories: impl WriteRepository + Clone,
    block_context_provider: Option<BlockContextProvider<impl L2Subpool>>,
    consensus_committed: Option<tokio::sync::mpsc::Receiver<BlockPayload>>,
    applied_block_number: (watch::Sender<Option<u64>>, watch::Receiver<Option<u64>>),
    tree: MerkleTree<RocksDBWrapper>,
    finality: impl ReadFinality + Clone,
    chain_id: u64,
    tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
    sidecar_sender: tokio::sync::mpsc::Sender<BlobTransactionSidecar>,
    committed_batch_provider: CommittedBatchProvider,
    canonization_engine: impl BlockCanonization,
    leadership: LeadershipSignal,
    stop_receiver: watch::Receiver<bool>,
    commit_submitted_tx: watch::Sender<u64>,
    verify_request_tx: tokio::sync::mpsc::Sender<VerifyBatch>,
    verify_result_rx: tokio::sync::mpsc::Receiver<PeerVerifyBatchResult>,
    verify_batch_rx: tokio::sync::mpsc::Receiver<PeerVerifyBatch>,
    outgoing_verify_results: tokio::sync::broadcast::Sender<PeerVerifyBatchResult>,
    pubdata_mode: PubdataMode,
    replay_archiver: Option<impl ReplayArchiver>,
    prebound_prover_api_listener: Option<TcpListener>,
) -> PipelineHandles {
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

    let monitor = BackpressureMonitor::new(config.build_backpressure_config(), stop_receiver);
    let pipeline_gate = monitor.subscribe_gate();

    let (applied_block_number_sender, applied_block_number_receiver) = applied_block_number;

    // Two front-ends produce the exact same stream of executed-and-canonical payloads:
    // consensus mode receives finalized blocks from the consensus environment; the
    // single-sequencer mode produces blocks locally behind a canonization fence.
    // Everything downstream of this point is identical.
    let pipeline = Pipeline::new(runtime.clone());
    let pipeline = if let Some(committed) = consensus_committed {
        pipeline.pipe(ConsensusCommittedSource {
            committed,
            block_replay_storage: block_replay_storage.clone(),
            starting_block,
            // The WAL tip has not moved since startup: nothing writes the WAL until
            // the pipeline's applier runs. This is the same height the consensus
            // environment resumed from, so live commits continue at exactly
            // `replay_until + 1`.
            replay_until: block_replay_storage.latest_record(),
            state: state.clone(),
            interop_roots_per_block: config.sequencer_config.interop_roots_per_block,
        })
    } else {
        let (replays_to_execute_sender, replays_to_execute) =
            tokio::sync::mpsc::unbounded_channel();
        pipeline
            .pipe(ConsensusNodeCommandSource {
                block_replay_storage: block_replay_storage.clone(),
                starting_block,
                rebuild_options,
                replays_to_execute,
                pipeline_gate,
                leadership,
            })
            .pipe(BlockExecutor {
                block_context_provider: block_context_provider
                    .expect("block context provider must exist without consensus"),
                state: state.clone(),
                config: config.into(),
                tx_acceptance_state_sender,
                applied_block_number_receiver,
            })
            .pipe(BlockCanonizer {
                consensus: canonization_engine,
                canonized_blocks_for_execution: replays_to_execute_sender,
            })
    };
    let pipeline = pipeline
        .pipe(BlockApplier {
            state: state.clone(),
            replay: block_replay_storage.clone(),
            repositories: repositories.clone(),
            config: config.into(),
            applied_block_number_sender,
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
        .pipe(TreeManager {
            tree: tree.clone(),
            runtime: runtime.clone(),
        });

    if !config.batcher_config.enabled {
        tracing::warn!(
            "Batcher subsystem disabled — skipping prover input generation, L1 settlement, and downstream components"
        );
        // A standby validator is a batch verifier: it co-signs the settler's
        // batch commitments against its own finalized chain (the committee is
        // the 2FA verifier set — no separate verifier fleet).
        let pipeline = pipeline.pipe_if(
            config.batch_verification_config.client_enabled,
            BatchVerificationResponder::new(
                chain_id,
                node_state_on_startup.l1_state.diamond_proxy_address(),
                config.batch_verification_config.signing_key.clone(),
                finality.clone(),
                node_state_on_startup.l1_state.clone(),
                state.clone(),
                verify_batch_rx,
                outgoing_verify_results,
            ),
            NoOpSink::new(),
        );
        let components = pipeline.components();
        pipeline.spawn();
        runtime.spawn_critical_task(
            "clear failing block config",
            clear_failing_block_config_task(finality, internal_config_manager),
        );
        let snapshot_rx = PipelineTracker::spawn(runtime, components);
        return PipelineHandles {
            backpressure_acceptance_rx: monitor.spawn(runtime, snapshot_rx.clone()),
            pipeline_snapshot_rx: snapshot_rx,
            prover_api_port: None,
        };
    }

    // The settler never verifies its own batches (a self-signature adds
    // nothing to 2FA), but peers may still probe: drain requests politely so
    // their sessions don't read a closed channel as a dead peer.
    runtime.spawn_critical_task("verify request drain", async move {
        let mut verify_batch_rx = verify_batch_rx;
        while let Some(request) = verify_batch_rx.recv().await {
            tracing::debug!(
                batch_number = request.message.batch_number,
                "ignoring verify request: this node settles, it does not co-sign",
            );
        }
    });

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

    let prover_api_port = if config.prover_api_config.enabled {
        let prover_listener = prebound_prover_api_listener
            .expect("prover API is enabled but prover API listener was not prebound");
        let port = prover_listener
            .local_addr()
            .expect("prover server local_addr")
            .port();
        runtime.spawn_critical_with_graceful_shutdown_signal("prover server", |shutdown| {
            prover_server::run(
                fri_job_manager.clone(),
                snark_job_manager.clone(),
                proof_storage.clone(),
                prover_listener,
                shutdown,
            )
        });
        Some(port)
    } else {
        None
    };

    if config.prover_api_config.fake_fri_provers.enabled {
        run_fake_fri_provers(&config.prover_api_config, runtime, fri_job_manager);
    }

    if config.prover_api_config.fake_snark_provers.enabled {
        run_fake_snark_provers(&config.prover_api_config, runtime, snark_job_manager);
    }

    if !config.prover_input_generator_config.enable_input_generation {
        assert!(
            config.prover_api_config.fake_fri_provers.enabled
                && config.prover_api_config.fake_snark_provers.enabled,
            "prover_input_generator_config.enable_input_generation=false requires both \
             prover_api_config.fake_fri_provers.enabled and \
             prover_api_config.fake_snark_provers.enabled to be true"
        );
    }

    let commit_sender_config: zksync_os_l1_sender::config::L1SenderConfig<CommitCommand> =
        config.l1_sender_config.clone().into();
    let prove_sender_config: zksync_os_l1_sender::config::L1SenderConfig<ProofCommand> =
        config.l1_sender_config.clone().into();
    let execute_sender_config: zksync_os_l1_sender::config::L1SenderConfig<ExecuteCommand> =
        config.l1_sender_config.clone().into();

    // The settler's on-chain identity. A committee runs exactly one settler at a
    // time, and a promoted standby settles with *its own* pre-authorized operator
    // addresses — so "who signs settlement right now" is the first question of
    // any settlement incident, answered here and by this node being the one with
    // the batcher running.
    let commit_operator = commit_sender_config
        .operator_signer
        .address()
        .await
        .expect("commit operator signer must resolve");
    let prove_operator = prove_sender_config
        .operator_signer
        .address()
        .await
        .expect("prove operator signer must resolve");
    let execute_operator = execute_sender_config
        .operator_signer
        .address()
        .await
        .expect("execute operator signer must resolve");
    tracing::info!(
        %commit_operator,
        %prove_operator,
        %execute_operator,
        "this node is the settler; settlement operator identities"
    );

    let pipeline = pipeline
        .pipe(ProverInputGenerator {
            enable_logging: config.prover_input_generator_config.logging_enabled,
            maximum_in_flight_blocks: config
                .prover_input_generator_config
                .maximum_in_flight_blocks,
            read_state: state.clone(),
            pubdata_mode,
            merkle_tree: tree,
            runtime: runtime.clone(),
            disabled: !config.prover_input_generator_config.enable_input_generation,
        })
        .pipe(Batcher {
            startup_config: BatcherStartupConfig {
                last_committed_batch: node_state_on_startup.l1_state.last_committed_batch,
                last_executed_batch: node_state_on_startup.l1_state.last_executed_batch,
                last_persisted_block: node_state_on_startup.block_replay_storage_last_block,
            },
            chain_id,
            // Feeds the committed `CommitBatchInfo.sl_chain_id` (part of the v31+ public input
            // hash); the settlement layer is always L1 now.
            sl_chain_id: node_state_on_startup.l1_state.l1_chain_id,
            chain_address: node_state_on_startup.l1_state.diamond_proxy_address(),
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
            verify_request_tx,
            verify_result_rx,
        ))
        .pipe(fri_proving_step)
        .pipe(GaplessCommitter {
            next_expected_batch_number: node_state_on_startup.l1_state.last_executed_batch + 1,
            last_committed_batch_number: node_state_on_startup.l1_state.last_committed_batch,
            proof_storage,
            batch_verification_l1_config: node_state_on_startup.l1_state.batch_verification.clone(),
        })
        .pipe(UpgradeGatekeeper::new(
            node_state_on_startup.l1_state.diamond_proxy_l1.clone(),
        ))
        .pipe_opt(replay_archiver.map(|replay_archiver| {
            ReplayArchiveGateComponent::new(replay_archiver, block_replay_storage.clone())
        }))
        .pipe(L1Sender::<CommitCommand> {
            provider: l1_provider.clone(),
            config: commit_sender_config,
            to_address: node_state_on_startup.l1_state.validator_timelock,
            commit_submitted_tx: Some(commit_submitted_tx),
            l1_block_number: node_state_on_startup.l1_state.l1_block_number,
        })
        .pipe(snark_proving_step)
        .pipe(GaplessL1ProofSender::new(
            node_state_on_startup.l1_state.last_executed_batch + 1,
        ))
        .pipe(L1Sender::<ProofCommand> {
            provider: l1_provider.clone(),
            config: prove_sender_config,
            to_address: node_state_on_startup.l1_state.validator_timelock,
            commit_submitted_tx: None,
            l1_block_number: node_state_on_startup.l1_state.l1_block_number,
        })
        .pipe(
            PriorityTreePipelineStep::new(
                block_replay_storage.clone(),
                &priority_tree_db_path,
                finality,
                committed_batch_provider,
            )
            .unwrap(),
        )
        .pipe(L1Sender {
            provider: l1_provider,
            config: execute_sender_config,
            to_address: node_state_on_startup.l1_state.validator_timelock,
            commit_submitted_tx: None,
            l1_block_number: node_state_on_startup.l1_state.l1_block_number,
        })
        .pipe(BatchSink::new(internal_config_manager));

    tracing::info!("Launching pipeline");
    let components = pipeline.components();
    pipeline.spawn();
    let snapshot_rx = PipelineTracker::spawn(runtime, components);
    PipelineHandles {
        backpressure_acceptance_rx: monitor.spawn(runtime, snapshot_rx.clone()),
        pipeline_snapshot_rx: snapshot_rx,
        prover_api_port,
    }
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
    runtime: &Runtime,
    block_context_provider: BlockContextProvider<impl L2Subpool>,
    state: impl ReadStateHistory + WriteState + Clone,
    tree: MerkleTree<RocksDBWrapper>,
    repositories: impl WriteRepository + Clone,
    finality: impl ReadFinality + Clone,
    stop_receiver: watch::Receiver<bool>,
    tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
    chain_id: u64,
    verify_batch_rx: tokio::sync::mpsc::Receiver<PeerVerifyBatch>,
    outgoing_verify_results: tokio::sync::broadcast::Sender<PeerVerifyBatchResult>,
    scheduled_cutover: Option<u64>,
) -> PipelineHandles {
    let internal_config_manager = init_and_report_internal_config_manager(
        config
            .general_config
            .rocks_db_path
            .join(INTERNAL_CONFIG_FILE_NAME),
    );
    let (applied_block_number_sender, applied_block_number_receiver) = watch::channel(None);

    let monitor =
        BackpressureMonitor::new(config.build_backpressure_config(), stop_receiver.clone());
    let pipeline_gate = monitor.subscribe_gate();

    let pipeline = Pipeline::new(runtime.clone())
        .pipe(ExternalNodeCommandSource {
            replays_for_sequencer,
            // A scheduled cutover caps replay at the consensus anchor, below
            // any operator-configured bound.
            up_to_block: config
                .sequencer_config
                .en_sync_up_to_block
                .into_iter()
                .chain(scheduled_cutover)
                .min(),
            pipeline_gate,
        })
        .pipe(BlockExecutor {
            block_context_provider,
            state: state.clone(),
            config: config.into(),
            tx_acceptance_state_sender,
            applied_block_number_receiver,
        })
        .pipe(BlockApplier {
            state: state.clone(),
            replay: block_replay_storage.clone(),
            repositories: repositories.clone(),
            config: config.into(),
            applied_block_number_sender,
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
        .pipe(TreeManager {
            tree: tree.clone(),
            runtime: runtime.clone(),
        })
        .pipe_if(
            config.batch_verification_config.client_enabled,
            BatchVerificationResponder::new(
                chain_id,
                node_state_on_startup.l1_state.diamond_proxy_address(),
                config.batch_verification_config.signing_key.clone(),
                finality.clone(),
                node_state_on_startup.l1_state.clone(),
                state.clone(),
                verify_batch_rx,
                outgoing_verify_results,
            ),
            NoOpSink::new(),
        );

    let components = pipeline.components();
    pipeline.spawn();
    let snapshot_rx = PipelineTracker::spawn(runtime, components);

    if config.general_config.run_priority_tree {
        let priority_tree_manager = PriorityTreeManager::new(
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
        .unwrap();
        runtime.spawn_critical_with_graceful_shutdown_signal(
            "priority tree caching",
            |shutdown| async move {
                let (reporter, _rx) =
                    zksync_os_observability::ComponentStateReporter::new("priority_tree");
                tokio::select! {
                    result = priority_tree_manager.run(None, reporter) => {
                        result.expect("PriorityTreeManager run failed");
                    }
                    _guard = shutdown => {
                    }
                }
            },
        );
    }
    runtime.spawn_critical_task(
        "clear failing block config",
        clear_failing_block_config_task(finality, internal_config_manager),
    );
    PipelineHandles {
        backpressure_acceptance_rx: monitor.spawn(runtime, snapshot_rx.clone()),
        pipeline_snapshot_rx: snapshot_rx,
        prover_api_port: None, // EN has no prover server
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

/// Returns the pubdata mode used by all block-producing components on the Main Node: the
/// configured `l1_sender.pubdata_mode` (its presence is enforced here).
fn effective_main_node_pubdata_mode(config: &Config) -> PubdataMode {
    config
        .l1_sender_config
        .pubdata_mode
        .expect("`l1_sender.pubdata_mode` is required on the Main Node")
}

/// Validates that the `l1_sender.operator_*_sk` keys required for the L1Sender pipeline are
/// present in config. Reports all missing keys at once via panic so the operator can fix them in
/// a single restart.
fn check_required_operator_keys(config: &Config) {
    let l1 = &config.l1_sender_config;
    let mut missing = vec![];
    if l1.operator_commit_sk.is_none() {
        missing.push("operator_commit_sk");
    }
    if l1.operator_prove_sk.is_none() {
        missing.push("operator_prove_sk");
    }
    if l1.operator_execute_sk.is_none() {
        missing.push("operator_execute_sk");
    }
    if !missing.is_empty() {
        let formatted = missing
            .iter()
            .map(|k| format!("`l1_sender.{k}`"))
            .collect::<Vec<_>>()
            .join(", ");
        panic!(
            "missing operator keys required for settling on L1: {formatted}. \
             Set them in the `l1_sender` config section."
        );
    }
}

async fn commit_proof_execute_block_numbers(
    l1_state: &L1State,
    committed_batch_provider: &CommittedBatchProvider,
) -> (u64, u64, u64, u64) {
    let last_committed_block = if l1_state.last_committed_batch == 0 {
        0
    } else {
        committed_batch_provider
            .get(l1_state.last_committed_batch)
            .expect("last_committed_batch is expected to be loaded")
            .last_block_number()
    };

    // only used to log on node startup
    let last_proved_block = if l1_state.last_proved_batch == 0 {
        0
    } else {
        committed_batch_provider
            .get(l1_state.last_proved_batch)
            .expect("last_proved_batch is expected to be loaded")
            .last_block_number()
    };

    let last_executed_block = if l1_state.last_executed_batch == 0 {
        0
    } else {
        committed_batch_provider
            .get(l1_state.last_executed_batch)
            .expect("last_executed_batch is expected to be loaded")
            .last_block_number()
    };
    let last_finalized_executed_block = if l1_state.last_finalized_executed_batch == 0 {
        0
    } else {
        committed_batch_provider
            .get(l1_state.last_finalized_executed_batch)
            .expect("last_finalized_executed_batch is expected to be loaded")
            .last_block_number()
    };
    (
        last_committed_block,
        last_proved_block,
        last_executed_block,
        last_finalized_executed_block,
    )
}

fn run_fake_snark_provers(
    config: &ProverApiConfig,
    runtime: &Runtime,
    snark_job_manager: Arc<SnarkJobManager>,
) {
    tracing::info!(
        max_batch_age = ?config.fake_snark_provers.max_batch_age,
        "Initializing fake SNARK prover"
    );
    let fake_snark_prover = FakeSnarkProver::new(
        snark_job_manager.clone(),
        config.fake_snark_provers.max_batch_age,
    );
    runtime.spawn_critical_task("fake snark prover", fake_snark_prover.run());
}

fn run_fake_fri_provers(
    config: &ProverApiConfig,
    runtime: &Runtime,
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
    fake_provers_pool.spawn(runtime);
}

/// Determines the block for node to start from.
///
/// Panics if no batches are committed to L1 yet.
fn determine_starting_block(
    config: &Config,
    node_startup_state: &NodeStateOnStartup,
    state: &impl ReadStateHistory,
    last_matching_block: BlockNumber,
    rebuild_options: Option<&RebuildOptions>,
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
            node_startup_state.last_l1_executed_block,
            // Repositories' persistence may have fallen behind - we need to replay blocks to rebuild it
            node_startup_state.repositories_persisted_block,
            // In the current tree implementation this will always be ahead of `last_l1_executed_block`,
            // but this may change if we make tree persistence async (like elsewhere)
            node_startup_state.tree_last_block,
            // For compacted state, we need to replay all blocks that were not persisted yet.
            // For FullDiffs state (default) - this is always ahead of `last_l1_executed_block`.
            *state.block_range_available().end(),
            // If block rebuild (aka block reversion) is configured, we should ensure we replay
            // all the blocks we are rebuilding
            rebuild_options.map_or(u64::MAX, |opts| opts.from_block_number),
        ]
        .into_iter()
        .min()
        .unwrap();

        if last_matching_block < want_to_start_from {
            tracing::warn!(
                last_matching_block,
                want_to_start_from,
                "Node's blocks diverged from main node's blocks. Starting from last matching block + 1."
            );
        }

        last_matching_block.min(want_to_start_from)
    };

    // Ignore genesis here as we never actually run it in sequencer
    if desired_starting_block > 0
        && desired_starting_block < state.block_range_available().start() + 1
    {
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
    main_node_client: &MainNodeClient,
) -> anyhow::Result<u64> {
    async fn check(
        repo: &RepositoryManager,
        main_node_client: &MainNodeClient,
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

    let last_block = repo.get_latest_block();
    // Check last block first. Unless there was a reorg recently, this should return quickly.
    if check(repo, main_node_client, last_block).await? {
        return Ok(last_block);
    }
    if !check(repo, main_node_client, 0).await? {
        panic!("Genesis block mismatch between EN and main node");
    }

    // Binary search for the last matching block.
    let mut left = 0u64;
    let mut right = last_block;
    while left < right {
        #[allow(clippy::manual_div_ceil)]
        let mid = (left + right + 1) / 2;
        if check(repo, main_node_client, mid).await? {
            left = mid;
        } else {
            right = mid - 1;
        }
    }
    Ok(left)
}

/// Returns whether the consensus engine's storage directory holds any engine
/// state. The `.instance-lock` file does not count: it is the mutual-exclusion
/// marker created before the journals ever exist (by the node's own startup,
/// and by the test harness's relaunch gate), so after an operator clears the
/// directory for a fork its reappearance must not read as a previous era's
/// leftovers. Absence is the only I/O error that means "fresh"; permission and
/// storage failures must stop startup instead of silently bypassing the
/// consensus-era guards.
fn consensus_engine_state_is_fresh(engine_dir: &Path) -> std::io::Result<bool> {
    let instance_lock = consensus::instance_lock_path(engine_dir);
    match std::fs::read_dir(engine_dir) {
        Ok(entries) => {
            for entry in entries {
                if entry?.path() != instance_lock {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err),
    }
}

/// The raft-based consensus prototype was removed in favor of the upcoming BFT consensus.
/// Nodes that ran it may still have its RocksDB directory on disk; it is no longer read or
/// written. We deliberately do not delete data on startup — this warning tells the operator
/// it is safe to remove by hand.
fn warn_on_leftover_raft_storage(config: &Config) {
    let raft_storage_path = config.general_config.rocks_db_path.join("raft");
    match raft_storage_path.try_exists() {
        Ok(true) => tracing::warn!(
            path = %raft_storage_path.display(),
            "found leftover storage of the removed raft consensus prototype; \
             it is unused and can be deleted"
        ),
        Ok(false) => {}
        Err(err) => tracing::warn!(
            path = %raft_storage_path.display(),
            %err,
            "failed to check for leftover raft consensus storage"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{check_batch_verification_mismatch, consensus_engine_state_is_fresh};
    use crate::config::BatchVerificationConfig;
    use alloy::primitives::address;
    use zksync_os_contract_interface::l1_discovery::{
        BatchVerificationSL, BatchVerificationSLConfig,
    };

    #[test]
    fn consensus_state_probe_ignores_the_instance_lock() {
        let root = tempfile::tempdir().expect("tempdir");
        let consensus = root.path().join("consensus");
        assert!(consensus_engine_state_is_fresh(&consensus).expect("absent"));

        std::fs::create_dir(&consensus).expect("create directory");
        assert!(consensus_engine_state_is_fresh(&consensus).expect("empty"));

        // The mutual-exclusion marker alone is not engine state: the node (and
        // the test harness's relaunch gate) create it before the guards run.
        std::fs::write(crate::consensus::instance_lock_path(&consensus), b"")
            .expect("write lock marker");
        assert!(consensus_engine_state_is_fresh(&consensus).expect("lock only"));

        std::fs::write(consensus.join("state"), b"present").expect("write state");
        assert!(!consensus_engine_state_is_fresh(&consensus).expect("nonempty"));
    }

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
