use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::time::Duration;
use zk_os_forward_system::run::BatchContext as BlockContext;
use zksync_os_l1_sender::commitment::CommitBatchInfo;
use zksync_os_types::{EncodableZksyncOs, L1Transaction, L2Transaction};

/// `BlockCommand`s drive the sequencer execution.
/// Produced by `CommandProducer` - first blocks are `Replay`ed from WAL
/// and then `Produce`d indefinitely.
///
/// Downstream transform:
/// `BlockTransactionProvider: (L1Mempool/L1Watcher, L2Mempool, BlockCommand) -> (PreparedBlockCommand)`
#[derive(Clone, Debug)]
pub enum BlockCommand {
    /// Replay a block from the WAL.
    Replay(ReplayRecord),
    /// Produce a new block from the mempool.
    /// Second argument - local seal criteria - target block time and max transaction number
    /// (Avoid container struct for now)
    Produce(BlockContext, (Duration, usize)),
}

/// Full data needed to replay a block - assuming storage is already in the correct state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRecord {
    pub block_context: BlockContext,
    pub l1_transactions: Vec<L1Transaction>,
    pub l2_transactions: Vec<L2Transaction>,
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
    pub tx_source: BoxStream<'static, UnifiedTransaction>,
    pub metrics_label: &'static str,
}

/// A unified transaction that can be either L1 or L2
/// We don't call this a "Transaction"
/// as for most purposes only `L2Transaction` should be considered a "Transaction".
/// todo: may get rid of it as we have EncodableZksyncOs trait
#[derive(Clone, Debug)]
pub enum UnifiedTransaction {
    L1(L1Transaction),
    L2(L2Transaction),
}

/// todo: may get rid of it as we have EncodableZksyncOs trait
impl EncodableZksyncOs for UnifiedTransaction {
    fn encode_zksync_os(self) -> Vec<u8> {
        match self {
            UnifiedTransaction::L1(tx) => tx.encode_zksync_os(),
            UnifiedTransaction::L2(tx) => tx.encode_zksync_os(),
        }
    }
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
    pub commit_batch_info: CommitBatchInfo,
}

impl BlockCommand {
    pub fn block_number(&self) -> u64 {
        match self {
            BlockCommand::Replay(record) => record.block_context.block_number,
            BlockCommand::Produce(context, _) => context.block_number,
        }
    }
}

impl Display for BlockCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockCommand::Replay(record) => write!(
                f,
                "Replay block {} ({} L1 txs, {} L2 txs)",
                record.block_context.block_number,
                record.l1_transactions.len(),
                record.l2_transactions.len()
            ),
            BlockCommand::Produce(context, duration) => write!(
                f,
                "Produce block {} target block properties: {:?}",
                context.block_number, duration
            ),
        }
    }
}
