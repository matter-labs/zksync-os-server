use crate::config::RaftConsensusConfig;
use crate::leadership_monitor::spawn_leadership_monitor;
use crate::model::{
    BlockCanonizationEngine, ConsensusBootstrapper, ConsensusNetworkProtocol, ConsensusRole,
    ConsensusRuntimeParts, ConsensusStatusSource, LeadershipSignal, OpenRaftCanonizationEngine,
};
use crate::network::{RaftNetworkFactory, RaftRpcHandler};
use crate::state_machine::RaftStateMachineStore;
use crate::status::RaftConsensusStatus;
use crate::storage::RaftLogStore;
use anyhow::Context;
use openraft::{Config, Raft, SnapshotPolicy};
use reth_network_peers::PeerId;
use std::collections::BTreeMap;
use tokio::sync::{mpsc, watch};
use zksync_os_consensus_types::RaftNode;
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
    config: RaftConsensusConfig,
    wal: Box<dyn ReadReplay>,
) -> anyhow::Result<ConsensusRuntimeParts> {
    anyhow::ensure!(
        config.peer_ids.contains(&config.node_id),
        "consensus.peer_ids does not include local peer id derived from network.secret_key: {}",
        config.node_id
    );

    let router = RaftRouter::default();
    let node_id = config.node_id;
    let raft_config = Config {
        cluster_name: "zksync-os-server".to_owned(),
        snapshot_policy: SnapshotPolicy::Never,
        election_timeout_max: config.election_timeout_max.as_millis() as u64,
        election_timeout_min: config.election_timeout_min.as_millis() as u64,
        heartbeat_interval: config.heartbeat_interval.as_millis() as u64,
        ..Default::default()
    };

    let raft_config = raft_config.validate().context("invalid raft config")?;

    let log_store = RaftLogStore::open(&config.storage_path)?;
    let (canonized_sender, canonized_rx) = mpsc::unbounded_channel();
    let state_machine = RaftStateMachineStore::new(log_store.db(), wal, canonized_sender);

    let nodes = peer_list_to_nodes(&config.peer_ids);
    let peer_ids: Vec<_> = nodes.keys().copied().collect();
    tracing::info!(
        %node_id,
        peers_count = config.peer_ids.len(),
        bootstrap = config.bootstrap,
        ?peer_ids,
        "initializing openraft consensus"
    );
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
    tracing::info!(%node_id, "openraft runtime created");
    let (leader_tx, leader_rx) = watch::channel(ConsensusRole::Replica);
    let (status_tx, status_rx) = watch::channel(RaftConsensusStatus {
        node_id: node_id.to_string(),
        state: "Learner".to_owned(),
        is_leader: false,
        current_leader: None,
        current_term: 0,
        last_applied_index: None,
    });
    spawn_leadership_monitor(raft.clone(), node_id.to_string(), leader_tx, status_tx);
    let rpc_handler = RaftRpcHandler::new(raft.clone());
    let protocol_handler = RaftProtocolHandler::new(rpc_handler, router.clone());

    Ok(ConsensusRuntimeParts {
        canonization_engine: BlockCanonizationEngine::OpenRaft(OpenRaftCanonizationEngine {
            raft: raft.clone(),
            canonized_blocks_rx: canonized_rx,
        }),
        leadership: LeadershipSignal::Watch(leader_rx),
        network_protocol: ConsensusNetworkProtocol::Raft(protocol_handler),
        bootstrapper: ConsensusBootstrapper::Raft(crate::bootstrap::RaftBootstrapper {
            raft: raft.clone(),
            bootstrap: config.bootstrap,
            router,
            node_id,
            peer_ids,
            membership_nodes: nodes,
        }),
        status: ConsensusStatusSource::Raft(status_rx),
    })
}

pub fn loopback_consensus() -> ConsensusRuntimeParts {
    ConsensusRuntimeParts {
        canonization_engine: BlockCanonizationEngine::Noop(NoopCanonization::new()),
        leadership: LeadershipSignal::AlwaysLeader,
        network_protocol: ConsensusNetworkProtocol::Disabled,
        bootstrapper: ConsensusBootstrapper::Noop,
        status: ConsensusStatusSource::None,
    }
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
        tracing::debug!(peer_id = %peer_id, "configured raft peer id");
    }
    nodes
}
