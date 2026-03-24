use vise::EncodeLabelValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "state", rename_all = "snake_case")]
pub enum GenericComponentState {
    WaitingRecv,
    Processing,
    // for multithreaded components,
    // we cannot effectively distinguish between Processing and Waiting for input,
    // as both happen simultaneously
    ProcessingOrWaitingRecv,
}

impl GenericComponentState {
    pub fn specific(&self) -> &'static str {
        match self {
            GenericComponentState::WaitingRecv => "waiting_recv",
            GenericComponentState::Processing => "processing",
            GenericComponentState::ProcessingOrWaitingRecv => "processing_or_waiting_recv",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::WaitingRecv => "waiting_recv",
            Self::Processing => "processing",
            Self::ProcessingOrWaitingRecv => "processing_or_waiting_recv",
        }
    }
}
