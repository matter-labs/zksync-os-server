use crate::model::{
    BlockCanonizationEngine, ConsensusBootstrapper, ConsensusNetworkProtocol,
    ConsensusRuntimeParts, ConsensusStatusSource, LeadershipSignal,
};
use zksync_os_sequencer::execution::NoopCanonization;

pub fn loopback_consensus() -> ConsensusRuntimeParts {
    ConsensusRuntimeParts {
        canonization_engine: BlockCanonizationEngine::Noop(NoopCanonization::new()),
        leadership: LeadershipSignal::AlwaysLeader,
        network_protocol: ConsensusNetworkProtocol::Disabled,
        bootstrapper: ConsensusBootstrapper::Noop,
        status: ConsensusStatusSource::None,
    }
}
