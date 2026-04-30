use std::collections::HashMap;
use std::time::Duration;

pub use zksync_os_pipeline::ComponentId;

const DEFAULT_BLOCK_DIFF_LIMIT: u64 = 100;
const DEFAULT_BATCH_DIFF_LIMIT: u64 = 1000;

/// Backpressure thresholds for a single component.
#[derive(Default, Clone, Debug)]
pub struct PipelineCondition {
    pub max_block_diff_to_upstream: Option<u64>,
    pub max_time_diff_to_upstream: Option<Duration>,
    /// Only meaningful for batch-pipeline components; forced to `None` for block-pipeline.
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
        | ComponentId::GaplessCommitter
        | ComponentId::UpgradeGatekeeper
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
        | ComponentId::BatchVerificationResponder
        | ComponentId::FriJobManager
        | ComponentId::SnarkJobManager => PipelineCondition::default(),
    }
}

impl BackpressureConfig {
    pub fn set(&mut self, id: ComponentId, condition: PipelineCondition) {
        self.conditions.insert(id, condition);
    }

    /// Returns the effective condition for `id`.
    ///
    /// Block-level pipeline stages default to `max_block_diff_to_upstream = 100` when not
    /// explicitly configured. Batch-level pipeline stages default to
    /// `max_batch_diff_to_upstream = 1000`. Pipeline sources, sinks, provers, and other
    /// non-participating components default to no threshold.
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
///
/// Excluded:
/// - Provers (`FriJobManager`, `SnarkJobManager`) are using downstream components
///   (`GaplessCommitter`, `GaplessL1ProofSender`) for the correct signals due to reordering.
/// - Pipeline sources (`ConsensusNodeCommandSource`, `ExternalNodeCommandSource`): no upstream.
/// - Pipeline sinks (`BatchSink`, `NoopSink`): no downstream.
/// - `BatchVerificationResponder`: conditional stage (`pipe_if`) — when disabled a `NoopSink`
///   takes its place, which shifts window pairs based on config. Also only reports block numbers
///   (no batch), so batch-diff pairs would always be `None`.
pub fn is_pipeline_stage(id: ComponentId) -> bool {
    !matches!(
        id,
        ComponentId::FriJobManager
            | ComponentId::SnarkJobManager
            | ComponentId::ConsensusNodeCommandSource
            | ComponentId::ExternalNodeCommandSource
            | ComponentId::BatchSink
            | ComponentId::NoopSink
            | ComponentId::BatchVerificationResponder
    )
}
