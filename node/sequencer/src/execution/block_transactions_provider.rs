use crate::model::{
    BlockCommand, InvalidTxPolicy, PreparedBlockCommand, ReplayRecord, SealPolicy,
    UnifiedTransaction,
};
use futures::StreamExt;
use zksync_os_mempool::{DynL1Pool, DynPool};

/// Component that turns `BlockCommand`s into `PreparedBlockCommand`s.
/// Last step in the stream where `Produce` and `Replay` are differentiated.
///
///  * Tracks L1 priority ID.
///  * Combines the L1 and L2 transactions
///  * Cross-checks L1 transactions in Replay blocks against L1 (important for ENs) todo: not implemented yet
pub struct BlockTransactionsProvider {
    next_l1_priority_id: u64,
    l1_mempool: DynL1Pool,
    l2_mempool: DynPool,
}

impl BlockTransactionsProvider {
    pub fn new(next_l1_priority_id: u64, l1_mempool: DynL1Pool, l2_mempool: DynPool) -> Self {
        Self {
            next_l1_priority_id,
            l1_mempool,
            l2_mempool,
        }
    }

    pub fn process_command(&mut self, block_command: BlockCommand) -> PreparedBlockCommand {
        // todo: validate next_l1_transaction_id by adding it directly to BlockCommand
        //  it's not clear whether we want to add it only to Replay or also in Produce

        match block_command {
            BlockCommand::Produce(block_context, (deadline, limit)) => {
                // Materialize L1 transactions from mempool to Vec<L1Transaction>
                let mut l1_transactions = Vec::new();

                // todo: maybe we need to limit their number here -
                //  relevant after extended downtime
                //  alternatively we can use the same appraoch as with l2 transactions -
                //  and just pass the stream (downstream will then consume as many as needed)
                while let Some(l1_tx) = self.l1_mempool.get(self.next_l1_priority_id) {
                    l1_transactions.push(l1_tx.clone());
                    self.next_l1_priority_id += 1;
                }

                // todo: This way we'll miss the priority transactions,
                //  if we need to seal the block before we process them all.
                //  we need to use the `notify_canonized_block` here
                //  just like we'll do that with reth mempool.

                // Create stream: L1 transactions first, then L2 transactions
                let l1_stream = futures::stream::iter(l1_transactions).map(UnifiedTransaction::L1);
                let l2_stream = Box::into_pin(self.l2_mempool.clone()).map(UnifiedTransaction::L2);
                PreparedBlockCommand {
                    block_context,
                    tx_source: Box::pin(l1_stream.chain(l2_stream)),
                    seal_policy: SealPolicy::Decide(deadline, limit),
                    invalid_tx_policy: InvalidTxPolicy::RejectAndContinue,
                    metrics_label: "produce",
                }
            }
            BlockCommand::Replay(ReplayRecord {
                block_context,
                l1_transactions,
                l2_transactions,
            }) => {
                // For replay, use pre-materialized transactions

                // todo: this shouldn't be needed because
                //  we need to report the last processed priority id upstream anyway
                l1_transactions
                    .iter()
                    .last()
                    .iter()
                    .for_each(|tx| self.next_l1_priority_id = tx.common_data.serial_id.0);

                let l1_stream = futures::stream::iter(l1_transactions).map(UnifiedTransaction::L1);
                let l2_stream = futures::stream::iter(l2_transactions).map(UnifiedTransaction::L2);

                PreparedBlockCommand {
                    block_context,
                    seal_policy: SealPolicy::UntilExhausted,
                    invalid_tx_policy: InvalidTxPolicy::Abort,
                    tx_source: Box::pin(l1_stream.chain(l2_stream)),
                    metrics_label: "replay",
                }
            }
        }
    }
}
