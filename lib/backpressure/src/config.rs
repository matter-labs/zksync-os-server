use std::collections::HashMap;
use std::time::Duration;

pub use zksync_os_pipeline::ComponentId;

pub const DEFAULT_BLOCK_DIFF_LIMIT: u64 = 256;
pub const DEFAULT_BATCH_DIFF_LIMIT: u64 = 128;

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
/// Global defaults apply to all block- and batch-pipeline components that do not have a
/// per-component override set via [`BackpressureConfig::set`].
#[derive(Clone, Debug)]
pub struct BackpressureConfig {
    conditions: HashMap<ComponentId, PipelineCondition>,
    pub default_block_diff_limit: u64,
    pub default_batch_diff_limit: u64,
}

impl Default for BackpressureConfig {
    fn default() -> Self {
        Self {
            conditions: HashMap::new(),
            default_block_diff_limit: DEFAULT_BLOCK_DIFF_LIMIT,
            default_batch_diff_limit: DEFAULT_BATCH_DIFF_LIMIT,
        }
    }
}

impl BackpressureConfig {
    fn default_condition_for(&self, id: ComponentId) -> PipelineCondition {
        match id {
            ComponentId::BlockCanonizer
            | ComponentId::BlockApplier
            | ComponentId::RevmConsistencyChecker
            | ComponentId::TreeManager
            | ComponentId::ProverInputGenerator => PipelineCondition {
                max_block_diff_to_upstream: Some(self.default_block_diff_limit),
                ..Default::default()
            },
            ComponentId::BatchVerification
            | ComponentId::FriJobManager
            | ComponentId::SnarkJobManager
            | ComponentId::GaplessCommitter
            | ComponentId::UpgradeGatekeeper
            | ComponentId::MigrationGate
            | ComponentId::ReplayArchiveGate
            | ComponentId::L1SenderCommit
            | ComponentId::L1SenderProve
            | ComponentId::L1SenderExecute
            | ComponentId::GaplessL1ProofSender
            | ComponentId::PriorityTree => PipelineCondition {
                max_batch_diff_to_upstream: Some(self.default_batch_diff_limit),
                ..Default::default()
            },
            ComponentId::ConsensusNodeCommandSource
            | ComponentId::ExternalNodeCommandSource
            | ComponentId::BlockExecutor
            | ComponentId::Batcher
            | ComponentId::BatchSink
            | ComponentId::NoopSink
            | ComponentId::BatchVerificationResponder => PipelineCondition::default(),
        }
    }

    pub fn set(&mut self, id: ComponentId, condition: PipelineCondition) {
        self.conditions.insert(id, condition);
    }

    /// Returns the effective condition for `id`.
    pub fn condition_for(&self, id: ComponentId) -> PipelineCondition {
        self.conditions
            .get(&id)
            .cloned()
            .unwrap_or_else(|| self.default_condition_for(id))
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
