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
    NoopSink,
    // External node — batch verification
    BatchVerificationResponder,
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
    // Both pipelines — optional consistency checker (registered when enabled, no backpressure)
    RevmConsistencyChecker,
}

impl ComponentId {
    /// Returns the component name as a snake_case string.
    ///
    /// **Must stay in sync with `rename_all = "snake_case"` on the `EncodeLabelValue` derive
    /// above.** If these diverge, Prometheus metrics and JSON API responses will use different
    /// names for the same component.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConsensusNodeCommandSource => "consensus_node_command_source",
            Self::ExternalNodeCommandSource => "external_node_command_source",
            Self::BlockExecutor => "block_executor",
            Self::BlockApplier => "block_applier",
            Self::TreeManager => "tree_manager",
            Self::BatchSink => "batch_sink",
            Self::NoopSink => "noop_sink",
            Self::BatchVerificationResponder => "batch_verification_responder",
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

    /// Stable display / reporting order for pipeline components.
    ///
    /// Numeric gaps are intentional so new stages can be inserted later without
    /// renumbering the full sequence in dashboards or tests.
    pub const fn pipeline_order(self) -> u64 {
        match self {
            Self::ConsensusNodeCommandSource => 0,
            Self::ExternalNodeCommandSource => 5,
            Self::BlockExecutor => 10,
            Self::BlockCanonizer => 20,
            Self::BlockApplier => 30,
            Self::RevmConsistencyChecker => 35,
            Self::TreeManager => 40,
            Self::ProverInputGenerator => 50,
            Self::Batcher => 60,
            Self::BatchVerification => 70,
            Self::FriJobManager => 80,
            Self::GaplessCommitter => 90,
            Self::UpgradeGatekeeper => 100,
            Self::L1SenderCommit => 110,
            Self::SnarkJobManager => 120,
            Self::GaplessL1ProofSender => 130,
            Self::L1SenderProve => 140,
            Self::PriorityTree => 150,
            Self::L1SenderExecute => 160,
            Self::BatchVerificationResponder => 170,
            Self::BatchSink => 180,
            Self::NoopSink => 190,
        }
    }
}
