use vise::{EncodeLabelValue, Metrics};
use zksync_os_observability::{GenericComponentState, StateLabel};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "seal_reason", rename_all = "snake_case")]
pub enum L1SenderState {
    WaitingRecv,
    WaitingSend,
    SendingToL1,
    WaitingL1Inclusion,
}

impl StateLabel for L1SenderState {
    fn generic(&self) -> GenericComponentState {
        match self {
            L1SenderState::WaitingRecv => GenericComponentState::WaitingRecv,
            L1SenderState::WaitingSend => GenericComponentState::WaitingSend,
            L1SenderState::SendingToL1 => GenericComponentState::Processing,
            L1SenderState::WaitingL1Inclusion => GenericComponentState::Processing,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            L1SenderState::WaitingRecv => "waiting_recv",
            L1SenderState::WaitingSend => "waiting_send",
            L1SenderState::SendingToL1 => "sending_to_l1",
            L1SenderState::WaitingL1Inclusion => "waiting_l1_inclusion",
        }
    }
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "l1_sender")]
pub struct L1SenderMetrics {}

#[vise::register]
pub static L1_SENDER_METRICS: vise::Global<L1SenderMetrics> = vise::Global::new();
