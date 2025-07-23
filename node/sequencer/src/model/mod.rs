use alloy::primitives::B256;
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::pin::Pin;
use std::time::Duration;
use zk_os_forward_system::run::BatchContext as BlockContext;
use zksync_os_l1_sender::commitment::CommitBatchInfo;
use zksync_os_mempool::TxStream;
use zksync_os_types::{ZkEnvelope, ZkTransaction};

type L1TxSerialId = u64;

/// `BlockCommand`s drive the sequencer execution.
/// Produced by `CommandProducer` - first blocks are `Replay`ed from WAL
/// and then `Produce`d indefinitely.
///
/// Downstream transform:
/// `BlockTransactionProvider: (L1Mempool/L1Watcher, L2Mempool, BlockCommand) -> (PreparedBlockCommand)`
#[derive(Clone, Debug)]
pub enum BlockCommand {
    /// Replay a block from the WAL.
    Replay(Box<ReplayRecord>),
    /// Produce a new block from the mempool.
    /// Second argument - local seal criteria - target block time and max transaction number
    /// (Avoid container struct for now)
    Produce(ProduceCommand),
}

/// Command to produce a new block.
#[derive(Clone, Debug)]
pub struct ProduceCommand {
    pub block_number: u64,
    pub block_time: Duration,
    pub max_transactions_in_block: usize,
}

/// Full data needed to replay a block - assuming storage is already in the correct state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRecord {
    pub block_context: BlockContext,
    /// L1 transaction serial id (0-based) expected at the beginning of this block.
    /// If `l1_transactions` is non-empty, equals to the first tx id in this block
    /// otherwise, `last_processed_l1_tx_id` equals to the previous block's value
    pub starting_l1_priority_id: L1TxSerialId,
    pub transactions: Vec<ZkTransaction>,
}

impl ReplayRecord {
    pub fn new(
        block_context: BlockContext,
        starting_l1_priority_id: L1TxSerialId,
        transactions: Vec<ZkTransaction>,
    ) -> Self {
        let first_l1_tx_priority_id = transactions.iter().find_map(|tx| match tx.envelope() {
            ZkEnvelope::L1(l1_tx) => Some(l1_tx.priority_id()),
            ZkEnvelope::L2(_) => None,
        });
        if let Some(first_l1_tx_priority_id) = first_l1_tx_priority_id {
            assert_eq!(
                first_l1_tx_priority_id, starting_l1_priority_id,
                "First L1 tx priority id must match next_l1_priority_id"
            );
        }
        assert!(
            !transactions.is_empty(),
            "Block must contain at least one tx"
        );
        Self {
            block_context,
            starting_l1_priority_id,
            transactions,
        }
    }
}

/// Better name wanted!
/// BlockCommand + Tx Source = PreparedBlockCommand
/// We use `BlockCommand` upstream (`CommandProducer`, `BlockTransactionProvider`),
/// while doing all preparations that depend on command type (replay vs produce).
/// Then we switch to `PreparedBlockCommand` in `BlockExecutor`,
/// which should handle them uniformly.
///
/// Downstream transform:
/// `BlockExecutor: (State, PreparedBlockCommand) -> (BlockOutput, ReplayRecord)`
pub struct PreparedBlockCommand {
    pub block_context: BlockContext,
    pub seal_policy: SealPolicy,
    pub invalid_tx_policy: InvalidTxPolicy,
    pub tx_source: Pin<Box<dyn TxStream<Item = ZkTransaction> + Send + 'static>>,
    /// L1 transaction serial id expected at the beginning of this block.
    /// Not used in execution directly, but required to construct ReplayRecord
    pub starting_l1_priority_id: L1TxSerialId,
    pub metrics_label: &'static str,
}

/// Behaviour when VM returns an InvalidTransaction error.
#[derive(Clone, Copy, Debug)]
pub enum InvalidTxPolicy {
    /// Invalid tx is skipped in block and discarded from mempool. Used when building a block.
    RejectAndContinue,
    /// Bubble the error up and abort the whole block. Used when replaying a block (ReplayLog / Replica / EN)
    Abort,
}

#[derive(Clone, Copy, Debug)]
pub enum SealPolicy {
    /// Seal non-empty blocks after deadline or N transactions. Used when building a block
    /// (Block Deadline, Block Size)
    Decide(Duration, usize),
    /// Seal when all txs from tx source are executed. Used when replaying a block (ReplayLog / Replica / EN)
    UntilExhausted,
}

/// Currently used both for prover api and eth-sender - may reconsider later on
pub struct BatchJob {
    pub block_number: u64,
    pub prover_input: Vec<u32>,
    pub previous_state_commitment: B256,
    pub commit_batch_info: CommitBatchInfo,
}

impl BlockCommand {
    pub fn block_number(&self) -> u64 {
        match self {
            BlockCommand::Replay(record) => record.block_context.block_number,
            BlockCommand::Produce(command) => command.block_number,
        }
    }
}

impl Display for BlockCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockCommand::Replay(record) => write!(
                f,
                "Replay block {} ({} txs); strating l1 priority id: {}",
                record.block_context.block_number,
                record.transactions.len(),
                record.starting_l1_priority_id,
            ),
            BlockCommand::Produce(command) => write!(f, "Produce block: {:?}", command),
        }
    }
}
