use zksync_os_observability::{GenericComponentState, StateLabel};

/// Component-specific state shared by `FriJobManager` and `SnarkJobManager`.
pub enum ProverJobManagerState {
    /// Queue empty — no work in flight.
    Idle,
    /// Jobs queued, awaiting submission from an external prover.
    WaitingForProver,
    /// Handling an incoming add/submit/send path.
    ProcessingSubmission,
}

impl StateLabel for ProverJobManagerState {
    fn generic(&self) -> GenericComponentState {
        match self {
            Self::Idle => GenericComponentState::Idle,
            Self::WaitingForProver => GenericComponentState::Active,
            Self::ProcessingSubmission => GenericComponentState::Active,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::WaitingForProver => "waiting_for_prover",
            Self::ProcessingSubmission => "processing_submission",
        }
    }
}
