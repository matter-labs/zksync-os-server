use async_trait::async_trait;
use futures::future;
use openraft::Raft;
use tokio::sync::{mpsc, watch};
use zksync_os_consensus_types::RaftTypeConfig;
use zksync_os_sequencer::execution::{BlockCanonization, NoopCanonization};
use zksync_os_storage_api::ReplayRecord;
use crate::bootstrap::RaftBootstrapper;
use crate::status::RaftConsensusStatus;
use zksync_os_network::raft::protocol::RaftProtocolHandler;

pub struct ConsensusRuntimeParts {
    pub canonization_engine: BlockCanonizationEngine,
    pub leadership: LeadershipSignal,
    pub network_protocol: ConsensusNetworkProtocol,
    pub bootstrapper: ConsensusBootstrapper,
    pub status: ConsensusStatusSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsensusRole {
    Leader,
    Replica,
}

pub struct OpenRaftCanonizationEngine {
    pub(crate) raft: Raft<RaftTypeConfig>,
    pub(crate) canonized_blocks_rx: mpsc::UnboundedReceiver<ReplayRecord>,
}

#[async_trait]
impl BlockCanonization for OpenRaftCanonizationEngine {
    async fn propose(&self, record: ReplayRecord) -> anyhow::Result<()> {
        self.raft.client_write(record).await?;
        Ok(())
    }

    async fn next_canonized(&mut self) -> anyhow::Result<ReplayRecord> {
        self.canonized_blocks_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("raft applied channel closed"))
    }
}

pub enum BlockCanonizationEngine {
    Noop(NoopCanonization),
    OpenRaft(OpenRaftCanonizationEngine),
}

#[async_trait]
impl BlockCanonization for BlockCanonizationEngine {
    async fn propose(&self, record: ReplayRecord) -> anyhow::Result<()> {
        match self {
            BlockCanonizationEngine::Noop(canonization) => canonization.propose(record).await,
            BlockCanonizationEngine::OpenRaft(canonization) => canonization.propose(record).await,
        }
    }

    async fn next_canonized(&mut self) -> anyhow::Result<ReplayRecord> {
        match self {
            BlockCanonizationEngine::Noop(canonization) => canonization.next_canonized().await,
            BlockCanonizationEngine::OpenRaft(canonization) => canonization.next_canonized().await,
        }
    }
}

#[derive(Debug, Clone)]
pub enum LeadershipSignal {
    AlwaysLeader,
    Watch(watch::Receiver<ConsensusRole>),
}

impl LeadershipSignal {
    pub fn current_role(&self) -> ConsensusRole {
        match self {
            Self::AlwaysLeader => ConsensusRole::Leader,
            Self::Watch(rx) => *rx.borrow(),
        }
    }

    pub async fn wait_for_change(&mut self) -> Result<(), watch::error::RecvError> {
        match self {
            Self::AlwaysLeader => future::pending::<Result<(), watch::error::RecvError>>().await,
            Self::Watch(rx) => rx.changed().await,
        }
    }
}

pub enum ConsensusNetworkProtocol {
    Disabled,
    Raft(RaftProtocolHandler),
}

impl ConsensusNetworkProtocol {
    pub fn into_protocol_handler(self) -> Option<RaftProtocolHandler> {
        match self {
            Self::Disabled => None,
            Self::Raft(handler) => Some(handler),
        }
    }
}

pub enum ConsensusBootstrapper {
    Noop,
    Raft(RaftBootstrapper),
}

impl ConsensusBootstrapper {
    pub async fn bootstrap_if_needed(&self) -> anyhow::Result<()> {
        match self {
            Self::Noop => Ok(()),
            Self::Raft(bootstrapper) => bootstrapper.bootstrap_if_needed().await,
        }
    }
}

pub enum ConsensusStatusSource {
    None,
    Raft(watch::Receiver<RaftConsensusStatus>),
}

impl ConsensusStatusSource {
    pub fn into_raft_status_rx(self) -> Option<watch::Receiver<RaftConsensusStatus>> {
        match self {
            Self::None => None,
            Self::Raft(rx) => Some(rx),
        }
    }
}
