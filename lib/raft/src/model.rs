use crate::bootstrap::RaftBootstrapper;
use crate::status::RaftConsensusStatus;
use async_trait::async_trait;
use futures::future;
use openraft::Raft;
use tokio::sync::{mpsc, watch};
use zksync_os_consensus_types::RaftTypeConfig;
use zksync_os_network::raft::protocol::RaftProtocolHandler;
use zksync_os_sequencer::execution::{BlockCanonization, NoopCanonization};
use zksync_os_storage_api::ReplayRecord;

pub struct ConsensusRuntimeParts {
    pub canonization_engine: BlockCanonizationEngine,
    pub leadership: LeadershipSignal,
    pub raft: Option<RaftRuntimeExtras>,
}

pub struct RaftRuntimeExtras {
    pub protocol_handler: RaftProtocolHandler,
    /// Present on nodes configured to attempt cluster bootstrap.
    pub bootstrapper: Option<RaftBootstrapper>,
    pub status_rx: watch::Receiver<Option<RaftConsensusStatus>>,
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
