use std::time::Duration;
use vise::{Buckets, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use vise::{Counter, EncodeLabelValue};
use zksync_os_observability::GenericComponentState;

const LATENCIES_FAST: Buckets = Buckets::exponential(0.0000001..=1.0, 2.0);
const STORAGE_WRITES: Buckets = Buckets::exponential(1.0..=1000.0, 1.7);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "seal_reason", rename_all = "snake_case")]
pub enum SealReason {
    Replay,
    Timeout,
    TxCountLimit,
    // Tx's gas limit + cumulative block gas > block gas limit - no execution attempt
    GasLimit,
    // VM returned `BlockGasLimitReached`
    GasVm,
    NativeCycles,
    Pubdata,
    L2ToL1Logs,
}

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

impl From<SequencerState> for GenericComponentState {
    fn from(val: SequencerState) -> Self {
        match val {
            SequencerState::WaitingForUpstreamCommand => GenericComponentState::WaitingRecv,
            SequencerState::WaitingForTx => GenericComponentState::WaitingRecv,

            SequencerState::SendingToBatcher => GenericComponentState::WaitingSend,
            SequencerState::SendingToTree => GenericComponentState::WaitingSend,

            _ => GenericComponentState::Processing,
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
