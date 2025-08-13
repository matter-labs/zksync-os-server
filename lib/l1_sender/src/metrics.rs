use vise::{Counter, EncodeLabelValue, LabeledFamily, Metrics};
use zksync_os_observability::GenericComponentState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "seal_reason", rename_all = "snake_case")]
pub enum L1SenderState {
    WaitingRecv,
    WaitingSend,
    SendingToL1,
    WaitingL1Inclusion,
}

impl From<L1SenderState> for GenericComponentState {
    fn from(val: L1SenderState) -> Self {
        match val {
            L1SenderState::WaitingRecv => GenericComponentState::WaitingRecv,
            L1SenderState::WaitingSend => GenericComponentState::WaitingSend,
            L1SenderState::SendingToL1 => GenericComponentState::Processing,
            L1SenderState::WaitingL1Inclusion => GenericComponentState::Processing,
        }
    }
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "l1_sender")]
pub struct L1SenderMetrics {
    #[metrics(labels = ["state"])]
    pub commit_state: LabeledFamily<L1SenderState, Counter<f64>>,
    #[metrics(labels = ["state"])]
    pub prove_state: LabeledFamily<L1SenderState, Counter<f64>>,
    #[metrics(labels = ["state"])]
    pub execute_state: LabeledFamily<L1SenderState, Counter<f64>>,
}

#[vise::register]
pub static L1_SENDER_METRICS: vise::Global<L1SenderMetrics> = vise::Global::new();
