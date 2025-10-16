use crate::execution::block_executor::SealReason;
use std::time::Duration;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use vise::{Counter, EncodeLabelValue};
use zksync_os_observability::{GenericComponentState, StateLabel};
use zksync_os_storage_api::StateAccessLabel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "state", rename_all = "snake_case")]
pub enum SequencerState {
    ConfiguredBlockLimitReached,

    WaitingForCommand,

    WaitingForTx,
    Execution,
    ReadStorage,
    ReadPreimage,
    Sealing,

    AddingToState,
    AddingToRepos,
    UpdatingMempool,
    AddingToWal,
    WaitingSend,
    BlockContextTxs,
    InitializingVm,
}
impl StateLabel for SequencerState {
    fn generic(&self) -> GenericComponentState {
        match self {
            Self::WaitingForCommand | Self::WaitingForTx | Self::ConfiguredBlockLimitReached => {
                GenericComponentState::WaitingRecv
            }
            Self::WaitingSend => GenericComponentState::WaitingSend,
            _ => GenericComponentState::Processing,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            SequencerState::ConfiguredBlockLimitReached => "configured_limit_reached",
            SequencerState::WaitingForCommand => "waiting_for_command",
            SequencerState::WaitingForTx => "waiting_for_tx",
            SequencerState::Execution => "execution",
            SequencerState::ReadStorage => "read_storage",
            SequencerState::ReadPreimage => "read_preimage",
            SequencerState::Sealing => "sealing",
            SequencerState::AddingToState => "adding_to_state",
            SequencerState::AddingToRepos => "adding_to_repos",
            SequencerState::UpdatingMempool => "updating_mempool",
            SequencerState::AddingToWal => "adding_to_wal",
            SequencerState::WaitingSend => "waiting_send",
            SequencerState::BlockContextTxs => "block_context_txs",
            SequencerState::InitializingVm => "initializing_vm",
        }
    }
}

impl StateAccessLabel for SequencerState {
    fn read_storage_state() -> Self {
        Self::ReadStorage
    }
    fn read_preimage_state() -> Self {
        Self::ReadPreimage
    }
    fn default_execution_state() -> Self {
        Self::Execution
    }
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "execution")]
pub struct ExecutionMetrics {
    #[metrics(labels = ["stage"])]
    pub block_number: LabeledFamily<&'static str, Gauge<u64>>,

    #[metrics(labels = ["seal_reason"])]
    pub seal_reason: LabeledFamily<SealReason, Counter>,

    #[metrics(unit = Unit::Seconds, labels = ["measure"], buckets = Buckets::exponential(0.0000001..=1.0, 2.0))]
    pub tx_execution: LabeledFamily<&'static str, Histogram<Duration>>,

    #[metrics(buckets = Buckets::exponential(1.0..=10_000.0, 2.0))]
    pub transactions_per_block: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=5_000_000.0, 4.0))]
    pub transaction_gas_used: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=50_000_000.0, 4.0))]
    pub transaction_native_used: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=50_000_000.0, 4.0))]
    pub transaction_computation_native_used: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(1.0..=1_000_000.0, 4.0))]
    pub transaction_pubdata_used: Histogram<u64>,

    #[metrics(labels = ["status"])]
    pub transaction_status: LabeledFamily<&'static str, Counter>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=1_000_000_000.0, 4.0))]
    pub computational_native_used_per_block: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(10_000.0..=100_000_000.0, 4.0))]
    pub gas_per_block: Histogram<u64>,

    #[metrics(buckets = Buckets::exponential(1_000.0..=500_000.0, 4.0))]
    pub pubdata_per_block: Histogram<u64>,

    pub executed_transactions: Counter,

    #[metrics(buckets = Buckets::exponential(1.0..=1_000.0, 1.7))]
    pub storage_writes_per_block: Histogram<u64>,

    pub next_l1_priority_id: Gauge<u64>,
}

#[vise::register]
pub(crate) static EXECUTION_METRICS: vise::Global<ExecutionMetrics> = vise::Global::new();
