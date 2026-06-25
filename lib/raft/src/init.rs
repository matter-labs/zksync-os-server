use crate::model::{BlockCanonizationEngine, ConsensusRuntimeParts, LeadershipSignal};
use zksync_os_sequencer::execution::NoopCanonization;

pub fn loopback_consensus() -> ConsensusRuntimeParts {
    ConsensusRuntimeParts {
        canonization_engine: BlockCanonizationEngine::Noop(NoopCanonization::new()),
        leadership: LeadershipSignal::AlwaysLeader,
    }
}
