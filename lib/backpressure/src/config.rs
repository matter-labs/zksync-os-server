use std::collections::HashMap;
use std::time::Duration;

pub use zksync_os_pipeline::ComponentId;

const DEFAULT_BLOCK_DIFF_LIMIT: u64 = 256;
const DEFAULT_BATCH_DIFF_LIMIT: u64 = 128;

/// Backpressure thresholds for a single component.
#[derive(Default, Clone, Debug)]
pub struct PipelineCondition {
    pub max_block_diff_to_upstream: Option<u64>,
    pub max_time_diff_to_upstream: Option<Duration>,
    pub max_batch_diff_to_upstream: Option<u64>,
}

/// Internal backpressure configuration — one optional threshold condition per component.
///
/// Presence in the map means the component has a threshold and can trigger backpressure.
#[derive(Clone, Debug, Default)]
pub struct BackpressureConfig {
    conditions: HashMap<ComponentId, PipelineCondition>,
}

fn default_condition_for(id: ComponentId) -> PipelineCondition {
    match id {
        ComponentId::BlockCanonizer
        | ComponentId::BlockApplier
        | ComponentId::RevmConsistencyChecker
        | ComponentId::TreeManager
        | ComponentId::ProverInputGenerator
        | ComponentId::Batcher => PipelineCondition {
            max_block_diff_to_upstream: Some(DEFAULT_BLOCK_DIFF_LIMIT),
            ..Default::default()
        },
        ComponentId::BatchVerification
        | ComponentId::FriJobManager
        | ComponentId::SnarkJobManager
        | ComponentId::GaplessCommitter
        | ComponentId::UpgradeGatekeeper
        | ComponentId::MigrationGate
        | ComponentId::L1SenderCommit
        | ComponentId::L1SenderProve
        | ComponentId::L1SenderExecute
        | ComponentId::GaplessL1ProofSender
        | ComponentId::PriorityTree => PipelineCondition {
            max_batch_diff_to_upstream: Some(DEFAULT_BATCH_DIFF_LIMIT),
            ..Default::default()
        },
        ComponentId::ConsensusNodeCommandSource
        | ComponentId::ExternalNodeCommandSource
        | ComponentId::BlockExecutor
        | ComponentId::BatchSink
        | ComponentId::NoopSink
        | ComponentId::BatchVerificationResponder => PipelineCondition::default(),
    }
}

impl BackpressureConfig {
    pub fn set(&mut self, id: ComponentId, condition: PipelineCondition) {
        self.conditions.insert(id, condition);
    }

    /// Returns the effective condition for `id`.
    pub fn condition_for(&self, id: ComponentId) -> PipelineCondition {
        self.conditions
            .get(&id)
            .cloned()
            .unwrap_or_else(|| default_condition_for(id))
    }
}

/// Returns whether a component participates in the adjacency window.
///
/// Window membership is topology-based. Excluded components are skipped
/// when computing adjacent pairs, so their neighbors become directly adjacent.
pub fn is_pipeline_stage(id: ComponentId) -> bool {
    !matches!(
        id,
        ComponentId::ConsensusNodeCommandSource
            | ComponentId::ExternalNodeCommandSource
            | ComponentId::BatchSink
            | ComponentId::NoopSink
            | ComponentId::BatchVerificationResponder
    )
}
