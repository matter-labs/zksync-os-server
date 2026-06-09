use vise::{EncodeLabelSet, EncodeLabelValue};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, EncodeLabelValue, EncodeLabelSet,
)]
#[metrics(rename_all = "snake_case", label = "component")]
pub enum ComponentId {
    ConsensusNodeCommandSource,
    ExternalNodeCommandSource,
    BlockExecutor,
    BlockApplier,
    TreeManager,
    BatchSink,
    NoopSink,
    BatchVerificationResponder,
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
    RevmConsistencyChecker,
    MigrationGate,
    ReplayArchiveGate,
}

impl ComponentId {
    /// Returns the component name as a snake_case string.
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
            Self::MigrationGate => "migration_gate",
            Self::ReplayArchiveGate => "replay_archive_gate",
        }
    }
}
