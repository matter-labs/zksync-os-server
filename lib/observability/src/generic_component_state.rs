use crate::StateLabel;
use vise::EncodeLabelValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "state", rename_all = "snake_case")]
pub enum GenericComponentState {
    WaitingRecv,
    Processing,
    WaitingSend,
    // for multithreaded components,
    // we cannot effectively distinguish between Processing and Waiting for input,
    // as both happen simultaneously
    ProcessingOrWaitingRecv,
}

impl StateLabel for GenericComponentState {
    fn generic(&self) -> GenericComponentState {
        *self
    }

    fn specific(&self) -> &'static str {
        match self {
            GenericComponentState::WaitingRecv => "waiting_recv",
            GenericComponentState::Processing => "processing",
            GenericComponentState::WaitingSend => "waiting_send",
            GenericComponentState::ProcessingOrWaitingRecv => "processing_or_waiting_send",
        }
    }
}
