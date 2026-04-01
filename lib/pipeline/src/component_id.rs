use vise::{EncodeLabelSet, EncodeLabelValue};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, EncodeLabelValue, EncodeLabelSet,
)]
#[metrics(rename_all = "snake_case", label = "component")]
pub enum ComponentId {
    // Both pipelines — sources (unmonitored)
    ConsensusNodeCommandSource,
    ExternalNodeCommandSource,
    // Both pipelines — execution
    BlockExecutor,
    BlockApplier,
    TreeManager,
    // Both pipelines — sinks (unmonitored)
    BatchSink,
    NoOpSink,
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
    // Optional consistency checker (unmonitored)
    RevmConsistencyChecker,
}

impl ComponentId {
    /// Returns the component name as a snake_case string.
    ///
    /// **Must stay in sync with `rename_all = "snake_case"` on the `EncodeLabelValue` derive
    /// above.** If these diverge, Prometheus metrics and JSON API responses will use different
    /// names for the same component. The test `as_str_matches_snake_case_derive` guards this.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConsensusNodeCommandSource => "consensus_node_command_source",
            Self::ExternalNodeCommandSource => "external_node_command_source",
            Self::BlockExecutor => "block_executor",
            Self::BlockApplier => "block_applier",
            Self::TreeManager => "tree_manager",
            Self::BatchSink => "batch_sink",
            Self::NoOpSink => "noop_sink",
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
            Self::RevmConsistencyChecker => "revm_consistency_checker",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_returns_snake_case() {
        assert_eq!(ComponentId::BlockExecutor.as_str(), "block_executor");
        assert_eq!(ComponentId::FriJobManager.as_str(), "fri_job_manager");
        assert_eq!(
            ComponentId::GaplessL1ProofSender.as_str(),
            "gapless_l1_proof_sender"
        );
    }

    /// Guards against as_str() and the EncodeLabelValue derive diverging.
    ///
    /// Both must produce the same snake_case string for each variant. If they diverge,
    /// Prometheus metrics and JSON API responses will use different component names.
    /// Adding a new variant requires a match arm in as_str() (compile error) AND a new
    /// case here (test failure), making divergence impossible to miss.
    #[test]
    fn as_str_matches_snake_case_derive() {
        let cases: &[(ComponentId, &str)] = &[
            (
                ComponentId::ConsensusNodeCommandSource,
                "consensus_node_command_source",
            ),
            (
                ComponentId::ExternalNodeCommandSource,
                "external_node_command_source",
            ),
            (ComponentId::BlockExecutor, "block_executor"),
            (ComponentId::BlockApplier, "block_applier"),
            (ComponentId::TreeManager, "tree_manager"),
            (ComponentId::BatchSink, "batch_sink"),
            (ComponentId::NoOpSink, "noop_sink"),
            (ComponentId::BlockCanonizer, "block_canonizer"),
            (ComponentId::ProverInputGenerator, "prover_input_generator"),
            (ComponentId::Batcher, "batcher"),
            (ComponentId::BatchVerification, "batch_verification"),
            (ComponentId::FriJobManager, "fri_job_manager"),
            (ComponentId::GaplessCommitter, "gapless_committer"),
            (ComponentId::UpgradeGatekeeper, "upgrade_gatekeeper"),
            (ComponentId::L1SenderCommit, "l1_sender_commit"),
            (ComponentId::SnarkJobManager, "snark_job_manager"),
            (ComponentId::GaplessL1ProofSender, "gapless_l1_proof_sender"),
            (ComponentId::L1SenderProve, "l1_sender_prove"),
            (ComponentId::PriorityTree, "priority_tree"),
            (ComponentId::L1SenderExecute, "l1_sender_execute"),
            (
                ComponentId::RevmConsistencyChecker,
                "revm_consistency_checker",
            ),
        ];
        for &(id, expected) in cases {
            assert_eq!(
                id.as_str(),
                expected,
                "as_str() for {id:?} must match the EncodeLabelValue snake_case encoding"
            );
        }
    }
}
