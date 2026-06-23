use crate::config::RaftConsensusConfig;
use crate::leadership_monitor::spawn_leadership_monitor;
use crate::model::{
    BlockCanonizationEngine, ConsensusRole, ConsensusRuntimeParts, LeadershipSignal,
    OpenRaftCanonizationEngine, RaftRuntimeExtras,
};
use crate::network::{RaftNetworkFactory, RaftRpcHandler};
use crate::state_machine::RaftStateMachineStore;
use crate::status::RaftConsensusStatus;
use crate::storage::{RaftLogStore, RaftStorageStartupState};
use anyhow::Context;
use openraft::{Config, Raft, SnapshotPolicy};
use reth_network_peers::PeerId;
use reth_tasks::Runtime;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use zksync_os_consensus_types::{RaftNode, RaftTypeConfig};
use zksync_os_network::raft::protocol::RaftProtocolHandler;
use zksync_os_network::raft::protocol::RaftRouter;
use zksync_os_sequencer::execution::NoopCanonization;
use zksync_os_storage_api::ReadReplay;

/// Initialises the OpenRaft consensus engine and returns the runtime parts needed by the node.
///
/// `wal` is a read-only handle to the block replay WAL. The state machine uses it to derive
/// the last applied `LogId` directly from `wal.latest_record()`, keeping it atomically
/// consistent with what `BlockApplier` has durably persisted: a block is only considered
/// applied once it is in the WAL.
pub async fn init_consensus(
    runtime: &Runtime,
    config: RaftConsensusConfig,
    block_replay_storage: Box<dyn ReadReplay>,
) -> anyhow::Result<ConsensusRuntimeParts> {
    let router = RaftRouter::default();
    let node_id = config.node_id;
    let raft_config = Config {
        cluster_name: "zksync-os-server".to_owned(),
        snapshot_policy: SnapshotPolicy::Never,
        election_timeout_max: config.election_timeout_max.as_millis() as u64,
        election_timeout_min: config.election_timeout_min.as_millis() as u64,
        heartbeat_interval: config.heartbeat_interval.as_millis() as u64,
        // Suppress the periodic election timer on startup; the startup election gate
        // re-enables it once we have proof we are not on a stale term. See
        // `spawn_startup_election_gate`. `Raft::initialize` (bootstrap) and
        // `Raft::trigger().elect()` are not gated by this flag, so first-cluster
        // formation still works.
        enable_elect: false,
        ..Default::default()
    };

    let raft_config = raft_config.validate().context("invalid raft config")?;

    let log_store = RaftLogStore::open(&config.storage_path)?;

    // Capture raw storage state BEFORE Raft::new() runs its reapply_committed() pass.
    // This lets us compare pre-init vs post-init to see whether any entries were replayed.
    let wal_last_block = block_replay_storage.latest_record();
    let storage_state: RaftStorageStartupState = log_store
        .startup_state(wal_last_block)
        .context("failed to read raft storage startup state")?;

    let (canonized_sender, canonized_rx) = mpsc::unbounded_channel();
    let state_machine =
        RaftStateMachineStore::new(log_store.db(), block_replay_storage, canonized_sender);

    let nodes = peer_list_to_nodes(&config.peer_ids);
    let peer_ids: Vec<_> = nodes.keys().copied().collect();
    let network_factory = RaftNetworkFactory::new(router.clone(), &nodes, &raft_config)
        .context("build raft network factory")?;

    let raft = Raft::new(
        config.node_id,
        std::sync::Arc::new(raft_config),
        network_factory,
        log_store,
        state_machine,
    )
    .await?;

    // Note: if wal_last_block was behind the committed index, Raft::new() may
    // have reapplied those logs by sending them to canonized_sender.
    let initial_metrics = raft.metrics().borrow().clone();
    let peers = config.peer_ids.len();
    let bootstrap = config.bootstrap;
    let raft_applied_for_wal_block = &storage_state.raft_applied_for_wal_block;
    let stored_vote = &storage_state.vote;
    let stored_committed = &storage_state.committed;
    let stored_last_log = &storage_state.last_log;
    let state = &initial_metrics.state;
    let current_term = initial_metrics.current_term;
    let vote = &initial_metrics.vote;
    let last_log_index = initial_metrics.last_log_index;
    let last_applied = &initial_metrics.last_applied;
    let purged = &initial_metrics.purged;
    tracing::info!(
        "openraft consensus initialized: node_id={node_id}, peers={peers}, bootstrap={bootstrap}, \
         wal_last_block={wal_last_block}, raft_applied_for_wal_block={raft_applied_for_wal_block:?}, \
         stored_vote={stored_vote:?}, stored_committed={stored_committed:?}, \
         stored_last_log={stored_last_log:?}, state={state:?}, current_term={current_term}, \
         vote={vote:?}, last_log_index={last_log_index:?}, last_applied={last_applied:?}, \
         purged={purged:?}",
    );

    spawn_startup_election_gate(runtime, raft.clone(), config.election_timeout_max);

    let (leader_tx, leader_rx) = watch::channel(ConsensusRole::Replica);
    let (status_tx, status_rx) = watch::channel::<Option<RaftConsensusStatus>>(None);
    spawn_leadership_monitor(
        runtime,
        raft.clone(),
        node_id.to_string(),
        leader_tx,
        status_tx,
    );
    let rpc_handler = RaftRpcHandler::new(raft.clone());
    let protocol_handler = RaftProtocolHandler::new(rpc_handler, router.clone());

    let bootstrapper = if config.bootstrap {
        Some(crate::bootstrap::RaftBootstrapper {
            raft: raft.clone(),
            router,
            node_id,
            peer_ids,
            membership_nodes: nodes,
        })
    } else {
        None
    };

    // OpenRaft spawns its core task with plain tokio::spawn, outside of reth_tasks.
    // Register an explicit shutdown task so that graceful_shutdown_with_timeout waits
    // for the RaftCore to finish — releasing its RocksDB handles — before returning.
    let shutdown_handle = raft.clone();
    runtime.spawn_critical_with_graceful_shutdown_signal("raft-shutdown", |shutdown| async move {
        let _ = shutdown.await;
        if let Err(e) = shutdown_handle.shutdown().await {
            tracing::warn!(%e, "raft shutdown error");
        }
    });

    Ok(ConsensusRuntimeParts {
        canonization_engine: BlockCanonizationEngine::OpenRaft(OpenRaftCanonizationEngine {
            raft,
            canonized_blocks_rx: canonized_rx,
        }),
        leadership: LeadershipSignal::Watch(leader_rx),
        raft: Some(RaftRuntimeExtras {
            protocol_handler,
            bootstrapper,
            status_rx,
        }),
    })
}

pub fn loopback_consensus() -> ConsensusRuntimeParts {
    ConsensusRuntimeParts {
        canonization_engine: BlockCanonizationEngine::Noop(NoopCanonization::new()),
        leadership: LeadershipSignal::AlwaysLeader,
        raft: None,
    }
}

/// Suppresses the periodic election timer until either:
///   * the node observes a current leader (proof its term is in sync with the cluster), or
///   * `3 * election_timeout_max` (clamped to a 5s floor) elapses without contact.
///
/// On the success path, the node has been brought into the current term via inbound
/// AppendEntries before it becomes eligible to start an election. This prevents a
/// just-restarted node carrying a stale persisted vote from pairing with another
/// stale-vote peer to form a transient phantom quorum on its old term — a scenario
/// that ends in the node being deposed within milliseconds and leaves the produce
/// pipeline parked (which is what `leadership_monitor.rs` then panics out of).
///
/// On the grace-expiry path, a fully restarted cluster still elects normally: every
/// node hits the same timer and `enable_elect` flips back on without input from
/// peers that are all in the same situation.
fn spawn_startup_election_gate(
    runtime: &Runtime,
    raft: Raft<RaftTypeConfig>,
    election_timeout_max: Duration,
) {
    let grace = (election_timeout_max * 3).max(Duration::from_secs(5));
    let mut metrics_rx = raft.metrics();
    runtime.spawn_critical_task("raft startup election gate", async move {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if metrics_rx.borrow().current_leader.is_some() {
                tracing::info!("startup election gate: leader contact established");
                break;
            }
            tokio::select! {
                changed = metrics_rx.changed() => {
                    if changed.is_err() {
                        // Raft engine shut down before we observed a leader; nothing
                        // to gate any more.
                        tracing::info!(
                            "startup election gate: raft metrics channel closed before contact"
                        );
                        return;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::info!(
                        "startup election gate: grace ({grace:?}) expired without leader contact"
                    );
                    break;
                }
            }
        }
        raft.runtime_config().elect(true);
    });
}

fn peer_list_to_nodes(peer_ids: &[PeerId]) -> BTreeMap<PeerId, RaftNode> {
    let mut nodes = BTreeMap::new();
    for peer_id in peer_ids {
        nodes.insert(
            *peer_id,
            RaftNode {
                addr: peer_id.to_string(),
            },
        );
        tracing::debug!("configured raft peer id: {peer_id}");
    }
    nodes
}
