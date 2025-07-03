use crate::model::BlockCommand;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use zksync_os_mempool::{DynL1Pool, DynPool};
use zksync_os_types::{L1Transaction, L2Transaction};

// todo: consider replacing with `Either` to prevent adding logic here
/// A unified transaction that can be either L1 or L2
/// Do NOT add logic to this enum - we want to treat L1 vs L2 transactions separately
/// everywhere but during the execution
#[derive(Clone, Debug)]
pub enum Transaction {
    L1(L1Transaction),
    L2(L2Transaction),
}

/// A stream of unified transactions
/// In current implementation, it always starts with L1 transactions followed by L2.
pub type UnifiedTxStream = Pin<Box<dyn Stream<Item = Transaction> + Send>>;

/// Component that prepares a transaction source for a given block command.
///  * Tracks L1 priority ID.
///  * Combines the L1 and L2 transactions
///  * Validates L1 transactions in Reply blocks against L1 state (todo: not implemented yet)
pub struct BlockTransactionsProvider {
    next_l1_priority_id: u64,
    l1_mempool: DynL1Pool,
    l2_mempool: DynPool,
}

impl BlockTransactionsProvider {
    pub fn new(
        next_l1_priority_id: u64,
        l1_mempool: DynL1Pool,
        l2_mempool: DynPool,
    ) -> Self {
        Self {
            next_l1_priority_id,
            l1_mempool,
            l2_mempool,
        }
    }

    /// Create a unified transaction stream for the given block command
    pub fn transaction_stream(&mut self, block_command: BlockCommand) -> UnifiedTxStream {
        // todo: validate next_l1_transaction_id
        // todo: it's not clear whether we want to add it only to Replay or also in Produce 
        match block_command {
            BlockCommand::Produce(_, _) => {
                // Materialize L1 transactions from mempool
                let mut l1_transactions = Vec::new();

                while let Some(l1_tx) = self.l1_mempool.get(self.next_l1_priority_id) {
                    l1_transactions.push(l1_tx.clone());
                    self.next_l1_priority_id += 1;
                }

                // todo: This way we'll miss the priority transactions,
                // todo: if we need to seal the block before we process them all.
                // todo: we need to use the `notify_canonized_block` here
                // todo: just like we'll do that with reth mempool.

                // Create stream: L1 transactions first, then L2 transactions
                let l1_stream = futures::stream::iter(l1_transactions).map(Transaction::L1);
                let l2_stream = Box::into_pin(self.l2_mempool.clone()).map(Transaction::L2);

                Box::pin(l1_stream.chain(l2_stream))
            }
            BlockCommand::Replay(replay) => {
                // For replay, use pre-materialized transactions
                replay.l1_transactions.iter().last().iter().for_each(|tx| self.next_l1_priority_id = tx.common_data.serial_id.0);
                
                let l1_stream = futures::stream::iter(replay.l1_transactions).map(Transaction::L1);
                let l2_stream = futures::stream::iter(replay.l2_transactions).map(Transaction::L2);

                Box::pin(l1_stream.chain(l2_stream))
            }
        }
    }
}
