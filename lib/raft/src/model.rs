use async_trait::async_trait;
use futures::future;
use tokio::sync::watch;
use zksync_os_sequencer::execution::{BlockCanonization, NoopCanonization};
use zksync_os_storage_api::ReplayRecord;

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

pub enum BlockCanonizationEngine {
    Noop(NoopCanonization),
}

#[async_trait]
impl BlockCanonization for BlockCanonizationEngine {
    async fn propose(&self, record: ReplayRecord) -> anyhow::Result<()> {
        match self {
            BlockCanonizationEngine::Noop(canonization) => canonization.propose(record).await,
        }
    }

    async fn next_canonized(&mut self) -> anyhow::Result<ReplayRecord> {
        match self {
            BlockCanonizationEngine::Noop(canonization) => canonization.next_canonized().await,
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
}

pub enum ConsensusBootstrapper {
    Noop,
}

pub enum ConsensusStatusSource {
    None,
}
