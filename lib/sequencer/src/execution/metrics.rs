use crate::execution::execute_block_in_vm::SealReason;
use std::time::Duration;
use vise::{Buckets, Counter, Gauge, Histogram, LabeledFamily, Metrics, Unit};
use zksync_os_observability::{GenericComponentState, StateLabel};
use zksync_os_storage_api::StateAccessLabel;

/// Component-specific state for the block executor / sequencer execution loop.
pub enum SequencerState {
    /// Waiting for the next block command from the command source.
    WaitingForCommand,
    /// `max_blocks_to_produce` limit reached — component is permanently halted.
    ConfiguredBlockLimitReached,
    /// Command dequeued, waiting for BlockApplier to finish applying the
    /// previous block.
    WaitingForApplier,
    /// Waiting for the first transaction to arrive in the mempool before block production can start.
    WaitingForTransaction,
    /// Setting up the VM for block execution.
    InitializingVm,
    /// Waiting for the next transaction from the tx stream.
    WaitingForTx,
    /// Running the VM to execute transactions.
    Execution,
    /// VM is performing a storage read.
    ReadStorage,
    /// VM is performing a preimage read.
    ReadPreimage,
    /// Updating the mempool after block execution.
    UpdatingMempool,
}

impl StateLabel for SequencerState {
    fn generic(&self) -> GenericComponentState {
        match self {
            Self::WaitingForCommand | Self::ConfiguredBlockLimitReached => {
                GenericComponentState::Idle
            }
            Self::WaitingForApplier | Self::WaitingForTransaction | Self::WaitingForTx => {
                GenericComponentState::Idle
            }
            Self::InitializingVm
            | Self::Execution
            | Self::ReadStorage
            | Self::ReadPreimage
            | Self::UpdatingMempool => GenericComponentState::Active,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            Self::WaitingForCommand => "waiting_for_command",
            Self::ConfiguredBlockLimitReached => "configured_block_limit_reached",
            Self::WaitingForApplier => "waiting_for_applier",
            Self::WaitingForTransaction => "waiting_for_transaction",
            Self::InitializingVm => "initializing_vm",
            Self::WaitingForTx => "waiting_for_tx",
            Self::Execution => "execution",
            Self::ReadStorage => "read_storage",
            Self::ReadPreimage => "read_preimage",
            Self::UpdatingMempool => "updating_mempool",
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

/// Component-specific state for the block canonizer.
pub enum BlockCanonizerState {
    /// Waiting in the outer select! — both consensus and executor arms live.
    Idle,
    /// `produced_queue` is full, so the executor-input arm is gated off. The
    /// component is only able to service the consensus arm until consensus
    /// drains at least one proposed block back to us.
    ProducedQueueFull,
    /// Consensus arm fired — processing a canonized block (either matched
    /// against a locally-produced block or forwarded to execution).
    HandlingConsensusBlock,
    /// Executor arm fired — processing a Replay block (forwarded downstream).
    HandlingExecutorBlock,
    /// Executor arm fired for a Produce/Rebuild block — awaiting consensus to
    /// accept the proposal.
    ProposingToConsensus,
}

impl StateLabel for BlockCanonizerState {
    fn generic(&self) -> GenericComponentState {
        match self {
            Self::Idle => GenericComponentState::Idle,
            Self::ProducedQueueFull => GenericComponentState::Active,
            Self::HandlingConsensusBlock
            | Self::HandlingExecutorBlock
            | Self::ProposingToConsensus => GenericComponentState::Active,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::ProducedQueueFull => "produced_queue_full",
            Self::HandlingConsensusBlock => "handling_consensus_block",
            Self::HandlingExecutorBlock => "handling_executor_block",
            Self::ProposingToConsensus => "proposing_to_consensus",
        }
    }
}

/// Component-specific state for the block applier.
pub enum BlockApplierState {
    /// Waiting for the next block from BlockCanonizer.
    Idle,
    /// Persisting replay record and applying storage writes to the state layer.
    AddingToStorage,
    /// Populating the repository layer used by the JSON-RPC API.
    PopulatingRepos,
}

impl StateLabel for BlockApplierState {
    fn generic(&self) -> GenericComponentState {
        match self {
            Self::Idle => GenericComponentState::Idle,
            _ => GenericComponentState::Active,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::AddingToStorage => "adding_to_storage",
            Self::PopulatingRepos => "populating_repos",
        }
    }
}

#[derive(Debug, Metrics)]
#[metrics(prefix = "execution")]
pub struct ExecutionMetrics {
    pub block_number: Gauge<u64>,

    #[metrics(unit = Unit::Seconds, buckets = Buckets::exponential(0.1..=600.0, 2.0))]
    pub time_since_last_block: Histogram<Duration>,

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

    pub last_execution_version: Gauge<u64>,

    pub pubdata_price: Gauge<u64>,

    pub blob_fill_ratio: Gauge<f64>,

    pub base_fee: Gauge<u64>,

    pub native_price: Gauge<u64>,
}

#[vise::register]
pub(crate) static EXECUTION_METRICS: vise::Global<ExecutionMetrics> = vise::Global::new();
