use vise::{Buckets, EncodeLabelValue, Gauge, Histogram, LabeledFamily, Metrics};
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
pub struct L1SenderMetrics {
    /// Used to report L1 operator addresses to Prometheus (commit/prove/execute),
    /// Gauge is always set to one.
    #[metrics(labels = ["operation", "operator_address"])]
    pub l1_operator_address: LabeledFamily<(&'static str, &'static str), Gauge, 2>,

    /// Operator wallet balance
    #[metrics(labels = ["command"])]
    pub balance: LabeledFamily<&'static str, Gauge<f64>>,

    /// Number of L1 transactions being sent in one batch (in parallel) - see `command_limit` config param.
    #[metrics(labels = ["command"])]
    pub parallel_transactions: LabeledFamily<&'static str, Gauge<u64>>,

    /// L1 Transaction fee in Ether (i.e. total cost of commit/prove/execute)
    #[metrics(labels = ["command"], buckets = Buckets::exponential(0.0001..=100.0, 3.0))]
    pub l1_transaction_fee_ether: LabeledFamily<&'static str, Histogram<f64>>,

    /// L1 Transaction fee in Ether per l2 transaction (`l1_transaction_fee / transactions_per_batch`)
    #[metrics(labels = ["command"], buckets = Buckets::exponential(0.0001..=100.0, 3.0))]
    pub l1_transaction_fee_per_l2_tx_ether: LabeledFamily<&'static str, Histogram<f64>>,

    /// Total L1 gas used by L1 transaction (i.e. commit/prove/execute)
    #[metrics(labels = ["command"], buckets = Buckets::exponential(1.0..=10_000_000.0, 3.0))]
    pub gas_used: LabeledFamily<&'static str, Histogram<u64>>,

    /// L1 gas used by L1 transaction per l2 transaction (`gas_used / transactions_per_batch`)
    #[metrics(labels = ["command"], buckets = Buckets::exponential(1.0..=10_000_000.0, 3.0))]
    pub gas_used_per_l2_tx: LabeledFamily<&'static str, Histogram<u64>>,

    /// Last nonce used
    #[metrics(labels = ["command"])]
    pub nonce: LabeledFamily<&'static str, Gauge<u64>>,
}

#[vise::register]
pub static L1_SENDER_METRICS: vise::Global<L1SenderMetrics> = vise::Global::new();
