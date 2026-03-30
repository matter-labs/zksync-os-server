use smart_config::{DescribeConfig, DeserializeConfig};
use std::time::Duration;
use vise::{EncodeLabelSet, EncodeLabelValue};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, EncodeLabelValue, EncodeLabelSet,
)]
#[metrics(rename_all = "snake_case", label = "component")]
pub enum ComponentId {
    // Both pipelines
    BlockExecutor,
    BlockApplier,
    TreeManager,
    // Main node — consensus
    BlockCanonizer,
    // Main node — proving and settlement
    ProverInputGenerator,
    Batcher,
    BatchVerification,
    FriJobManager,
    GaplessCommitter,
    UpgradeGatekeeper,
    L1SenderCommit,
    SnarkJobManager,
    GaplessL1ProofSender,
    L1SenderProve,
    PriorityTree,
    L1SenderExecute,
}

impl ComponentId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BlockExecutor => "block_executor",
            Self::BlockApplier => "block_applier",
            Self::TreeManager => "tree_manager",
            Self::BlockCanonizer => "block_canonizer",
            Self::ProverInputGenerator => "prover_input_generator",
            Self::Batcher => "batcher",
            Self::BatchVerification => "batch_verification",
            Self::FriJobManager => "fri_job_manager",
            Self::GaplessCommitter => "gapless_committer",
            Self::UpgradeGatekeeper => "upgrade_gatekeeper",
            Self::L1SenderCommit => "l1_sender_commit",
            Self::SnarkJobManager => "snark_job_manager",
            Self::GaplessL1ProofSender => "gapless_l1_proof_sender",
            Self::L1SenderProve => "l1_sender_prove",
            Self::PriorityTree => "priority_tree",
            Self::L1SenderExecute => "l1_sender_execute",
        }
    }
}

/// Per-component backpressure thresholds.
/// Both conditions are independent — either can trigger backpressure.
#[derive(DescribeConfig, DeserializeConfig, Default, Clone, Debug)]
pub struct BackpressureCondition {
    /// Trigger backpressure when this component is more than N blocks behind BlockExecutor.
    pub max_block_lag: Option<u64>,
    /// Trigger backpressure when block-timestamp lag exceeds this duration.
    /// Only evaluated when `last_processed_block_timestamp` is `Some` for both head and this component.
    pub max_time_lag: Option<Duration>,
}

/// Per-component backpressure config.
/// A separate metrics_interval controls how often Prometheus gauges are refreshed.
///
/// `default` acts as a fallback: any per-component field that is `None` is resolved
/// from `default`. This lets operators set a single `pipeline_health.default.max_block_lag`
/// without needing to enumerate every component individually. For most deployments,
/// setting only `default` is sufficient; per-component fields allow advanced per-component tuning.
#[derive(DescribeConfig, DeserializeConfig, Clone, Debug)]
#[config(derive(Default))]
pub struct PipelineHealthConfig {
    /// Fallback thresholds applied to any component that does not have its own explicit setting.
    #[config(nest, default)]
    pub default: BackpressureCondition,
    #[config(nest, default)]
    pub block_executor: BackpressureCondition,
    #[config(nest, default)]
    pub block_applier: BackpressureCondition,
    #[config(nest, default)]
    pub tree_manager: BackpressureCondition,
    #[config(nest, default)]
    pub block_canonizer: BackpressureCondition,
    #[config(nest, default)]
    pub prover_input_generator: BackpressureCondition,
    #[config(nest, default)]
    pub batcher: BackpressureCondition,
    #[config(nest, default)]
    pub batch_verification: BackpressureCondition,
    #[config(nest, default)]
    pub fri_job_manager: BackpressureCondition,
    #[config(nest, default)]
    pub gapless_committer: BackpressureCondition,
    #[config(nest, default)]
    pub upgrade_gatekeeper: BackpressureCondition,
    #[config(nest, default)]
    pub l1_sender_commit: BackpressureCondition,
    #[config(nest, default)]
    pub snark_job_manager: BackpressureCondition,
    #[config(nest, default)]
    pub gapless_l1_proof_sender: BackpressureCondition,
    #[config(nest, default)]
    pub l1_sender_prove: BackpressureCondition,
    #[config(nest, default)]
    pub priority_tree: BackpressureCondition,
    #[config(nest, default)]
    pub l1_sender_execute: BackpressureCondition,
    /// How often to refresh Prometheus metrics regardless of health change events.
    /// Default: 5 seconds.
    #[config(default_t = Duration::from_secs(5))]
    pub metrics_interval: Duration,
}

impl PipelineHealthConfig {
    /// Returns the effective backpressure condition for `id`.
    ///
    /// Per-component settings take precedence; any field left `None` falls back to `self.default`.
    pub fn condition_for(&self, id: ComponentId) -> BackpressureCondition {
        let specific = match id {
            ComponentId::BlockExecutor => &self.block_executor,
            ComponentId::BlockApplier => &self.block_applier,
            ComponentId::TreeManager => &self.tree_manager,
            ComponentId::BlockCanonizer => &self.block_canonizer,
            ComponentId::ProverInputGenerator => &self.prover_input_generator,
            ComponentId::Batcher => &self.batcher,
            ComponentId::BatchVerification => &self.batch_verification,
            ComponentId::FriJobManager => &self.fri_job_manager,
            ComponentId::GaplessCommitter => &self.gapless_committer,
            ComponentId::UpgradeGatekeeper => &self.upgrade_gatekeeper,
            ComponentId::L1SenderCommit => &self.l1_sender_commit,
            ComponentId::SnarkJobManager => &self.snark_job_manager,
            ComponentId::GaplessL1ProofSender => &self.gapless_l1_proof_sender,
            ComponentId::L1SenderProve => &self.l1_sender_prove,
            ComponentId::PriorityTree => &self.priority_tree,
            ComponentId::L1SenderExecute => &self.l1_sender_execute,
        };
        BackpressureCondition {
            max_block_lag: specific.max_block_lag.or(self.default.max_block_lag),
            max_time_lag: specific.max_time_lag.or(self.default.max_time_lag),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_conditions_are_all_none() {
        let config = PipelineHealthConfig::default();
        let cond = config.condition_for(ComponentId::BlockExecutor);
        assert!(cond.max_block_lag.is_none());
        assert!(cond.max_time_lag.is_none());
    }

    #[test]
    fn default_fallback_applies_when_component_is_unset() {
        let config = PipelineHealthConfig {
            default: BackpressureCondition {
                max_block_lag: Some(50),
                max_time_lag: None,
            },
            ..Default::default()
        };
        let cond = config.condition_for(ComponentId::BlockApplier);
        assert_eq!(cond.max_block_lag, Some(50));
    }

    #[test]
    fn per_component_overrides_default() {
        let config = PipelineHealthConfig {
            default: BackpressureCondition {
                max_block_lag: Some(50),
                max_time_lag: None,
            },
            block_applier: BackpressureCondition {
                max_block_lag: Some(10),
                max_time_lag: None,
            },
            ..Default::default()
        };
        let cond = config.condition_for(ComponentId::BlockApplier);
        assert_eq!(cond.max_block_lag, Some(10));
    }

    #[test]
    fn condition_for_all_variants() {
        let config = PipelineHealthConfig::default();
        use ComponentId::*;
        for id in [
            BlockExecutor,
            BlockApplier,
            TreeManager,
            BlockCanonizer,
            ProverInputGenerator,
            Batcher,
            BatchVerification,
            FriJobManager,
            GaplessCommitter,
            UpgradeGatekeeper,
            L1SenderCommit,
            SnarkJobManager,
            GaplessL1ProofSender,
            L1SenderProve,
            PriorityTree,
            L1SenderExecute,
        ] {
            let _ = config.condition_for(id);
        }
    }

    #[test]
    fn as_str_returns_snake_case() {
        assert_eq!(ComponentId::BlockExecutor.as_str(), "block_executor");
        assert_eq!(ComponentId::FriJobManager.as_str(), "fri_job_manager");
        assert_eq!(
            ComponentId::GaplessL1ProofSender.as_str(),
            "gapless_l1_proof_sender"
        );
    }

    #[test]
    fn metrics_interval_default_is_five_seconds() {
        let config = PipelineHealthConfig::default();
        assert_eq!(config.metrics_interval, Duration::from_secs(5));
    }
}
