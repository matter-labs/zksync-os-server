use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::time::Duration;
use zk_os_forward_system::run::BatchContext;
use zksync_os_l1_sender::commitment::CommitBatchInfo;
use zksync_os_types::{L1Transaction, L2Transaction};

#[derive(Clone, Debug)]
pub enum BlockCommand {
    /// Replay a block from the WAL.
    Replay(ReplayRecord),
    /// Produce a new block from the mempool.
    /// Second argument - target block time.
    Produce(BatchContext, Duration),
}

impl BlockCommand {
    pub fn block_number(&self) -> u64 {
        match self {
            BlockCommand::Replay(record) => record.context.block_number,
            BlockCommand::Produce(context, _) => context.block_number,
        }
    }
}

/// Full data needed to replay a block - assuming storage is already in the correct state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRecord {
    pub context: BatchContext,
    pub l1_transactions: Vec<L1Transaction>,
    pub l2_transactions: Vec<L2Transaction>,
}

pub enum TransactionSource {
    Replay(Vec<L1Transaction>, Vec<L2Transaction>),
    Mempool,
}

/// Currently used both for prover api and eth-sender - may reconsider later on
pub struct BatchJob {
    pub block_number: u64,
    pub prover_input: Vec<u32>,
    pub commit_batch_info: CommitBatchInfo,
}

impl Display for BlockCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockCommand::Replay(record) => write!(
                f,
                "Replay block {} ({} L1 txs, {} L2 txs)",
                record.context.block_number,
                record.l1_transactions.len(),
                record.l2_transactions.len()
            ),
            BlockCommand::Produce(context, duration) => write!(
                f,
                "Produce block {} target block time: {:?}",
                context.block_number, duration
            ),
        }
    }
}
