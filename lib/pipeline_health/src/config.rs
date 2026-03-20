use smart_config::{DescribeConfig, DeserializeConfig};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

    /// Whether this component is reactive (never holds WaitingSend for measurable duration).
    /// Only FriJobManager uses try_reserve instead of blocking .await in WaitingSend.
    pub fn is_reactive(self) -> bool {
        matches!(self, Self::FriJobManager)
    }
}

#[derive(DescribeConfig, DeserializeConfig, Default, Clone, Debug)]
pub struct BackpressureCondition {
    pub max_waiting_send_duration: Option<Duration>,
    pub max_block_lag: Option<u64>,
}

#[derive(DescribeConfig, DeserializeConfig, Clone, Debug)]
#[config(derive(Default))]
pub struct PipelineHealthConfig {
    #[config(default_t = Duration::from_secs(1))]
    pub eval_interval: Duration,
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
}

impl PipelineHealthConfig {
    pub fn condition_for(&self, id: ComponentId) -> &BackpressureCondition {
        match id {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn default_config_has_one_second_interval() {
        let config = PipelineHealthConfig::default();
        assert_eq!(config.eval_interval, Duration::from_secs(1));
    }

    #[test]
    fn default_conditions_are_all_none() {
        let config = PipelineHealthConfig::default();
        let cond = config.condition_for(ComponentId::BlockExecutor);
        assert!(cond.max_waiting_send_duration.is_none());
        assert!(cond.max_block_lag.is_none());
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
    fn fri_job_manager_is_reactive_others_are_not() {
        assert!(ComponentId::FriJobManager.is_reactive());
        assert!(!ComponentId::BlockExecutor.is_reactive());
        assert!(!ComponentId::SnarkJobManager.is_reactive());
        assert!(!ComponentId::L1SenderCommit.is_reactive());
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
}
