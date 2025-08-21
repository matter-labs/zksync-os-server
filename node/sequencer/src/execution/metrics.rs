use crate::execution::block_executor::SealReason;
use std::time::Duration;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use vise::{Counter, EncodeLabelValue};
use zksync_os_observability::{GenericComponentState, StateLabel};

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
const STORAGE_WRITES: Buckets = Buckets::exponential(1.0..=1000.0, 1.7);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "state", rename_all = "snake_case")]
pub enum SequencerState {
    WaitingForUpstreamCommand,

    WaitingForTx,
    VmAndStorage,
    Sealing,

    AddingToState,
    AddingToRepos,
    UpdatingMempool,
    AddingToWal,
    SendingToBatcher,
    SendingToTree,
    PreparingBlockCommand,
    InitializingVm,
}
impl StateLabel for SequencerState {
    fn generic(&self) -> GenericComponentState {
        match self {
            SequencerState::WaitingForUpstreamCommand => GenericComponentState::WaitingRecv,
            SequencerState::WaitingForTx => GenericComponentState::WaitingRecv,

            SequencerState::SendingToBatcher => GenericComponentState::WaitingSend,
            SequencerState::SendingToTree => GenericComponentState::WaitingSend,

            _ => GenericComponentState::Processing,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            SequencerState::WaitingForUpstreamCommand => "waiting_for_upstream_command",
            SequencerState::WaitingForTx => "waiting_for_tx",
            SequencerState::VmAndStorage => "vm_and_storage",
            SequencerState::Sealing => "sealing",
            SequencerState::AddingToState => "adding_to_state",
            SequencerState::AddingToRepos => "adding_to_repos",
            SequencerState::UpdatingMempool => "updating_mempool",
            SequencerState::AddingToWal => "adding_to_wal",
            SequencerState::SendingToBatcher => "sending_to_batcher",
            SequencerState::SendingToTree => "sending_to_tree",
            SequencerState::PreparingBlockCommand => "preparing_block_command",
            SequencerState::InitializingVm => "initializing_vm",
        }
    }
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "execution")]
pub struct ExecutionMetrics {
    #[metrics(labels = ["stage"])]
    pub block_number: LabeledFamily<&'static str, Gauge<u64>>,

    pub executed_transactions: Counter,

    #[metrics(labels = ["state"])]
    pub block_execution_stages: LabeledFamily<SequencerState, Counter<f64>>,

    #[metrics(buckets = STORAGE_WRITES)]
    pub storage_writes_per_block: Histogram<u64>,

    pub next_l1_priority_id: Gauge<u64>,

    #[metrics(labels = ["seal_reason"])]
    pub seal_reason: LabeledFamily<SealReason, Counter>,
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "block_replay_storage")]
pub struct BlockReplayRocksDBMetrics {
    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub get_latency: Histogram<Duration>,

    #[metrics(unit = Unit::Seconds, buckets = LATENCIES_FAST)]
    pub set_latency: Histogram<Duration>,
}

#[vise::register]
pub(crate) static EXECUTION_METRICS: vise::Global<ExecutionMetrics> = vise::Global::new();

#[vise::register]
pub(crate) static BLOCK_REPLAY_ROCKS_DB_METRICS: vise::Global<BlockReplayRocksDBMetrics> =
    vise::Global::new();
