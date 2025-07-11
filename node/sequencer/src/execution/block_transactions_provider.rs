use crate::model::{BlockCommand, InvalidTxPolicy, PreparedBlockCommand, ReplayRecord, SealPolicy};
use crate::reth_state::ZkClient;
use alloy::consensus::{Block, BlockBody, Header};
use alloy::primitives::BlockHash;
use futures::StreamExt;
use reth_primitives::SealedBlock;
use zk_os_forward_system::run::BatchOutput;
use zksync_os_mempool::{
    CanonicalStateUpdate, DynL1Pool, L2TransactionPool, PoolUpdateKind, RethPool,
    RethTransactionPoolExt,
};
use zksync_os_types::{L2Envelope, ZkEnvelope, ZkTransaction};

/// Component that turns `BlockCommand`s into `PreparedBlockCommand`s.
/// Last step in the stream where `Produce` and `Replay` are differentiated.
///
///  * Tracks L1 priority ID.
///  * Combines the L1 and L2 transactions
///  * Cross-checks L1 transactions in Replay blocks against L1 (important for ENs) todo: not implemented yet
///
/// Note: unlike other components, this one doesn't tolerate replaying blocks -
///  it doesn't tolerate jumps in L1 priority IDs.
///  this is easily fixable if needed.
pub struct BlockTransactionsProvider {
    next_l1_priority_id: u64,
    l1_mempool: DynL1Pool,
    l2_mempool: RethPool<ZkClient>,
}

impl BlockTransactionsProvider {
    pub fn new(
        next_l1_priority_id: u64,
        l1_mempool: DynL1Pool,
        l2_mempool: RethPool<ZkClient>,
    ) -> Self {
        Self {
            next_l1_priority_id,
            l1_mempool,
            l2_mempool,
        }
    }

    pub fn process_command(
        &mut self,
        block_command: BlockCommand,
    ) -> anyhow::Result<PreparedBlockCommand> {
        // todo: validate next_l1_transaction_id by adding it directly to BlockCommand
        //  it's not clear whether we want to add it only to Replay or also in Produce

        match block_command {
            BlockCommand::Produce(block_context, (deadline, limit)) => {
                // Materialize L1 transactions from mempool to Vec<L1Transaction>
                let mut l1_transactions = Vec::new();

                let starting_l1_priority_id = self.next_l1_priority_id;
                let mut next_l1_priority_id = self.next_l1_priority_id;

                // todo: maybe we need to limit their number here -
                //  relevant after extended downtime
                //  alternatively we can use the same approach as with l2 transactions -
                //  and just pass the stream (downstream will then consume as many as needed)
                while let Some(l1_tx) = self.l1_mempool.get(next_l1_priority_id) {
                    l1_transactions.push(l1_tx.clone());

                    anyhow::ensure!(
                        l1_tx.priority_id() == next_l1_priority_id,
                        "L1 priority ID mismatch: expected {}, got {}",
                        next_l1_priority_id,
                        l1_tx.priority_id()
                    );

                    next_l1_priority_id += 1;
                }

                // Create stream: L1 transactions first, then L2 transactions
                let l1_stream = futures::stream::iter(l1_transactions).map(ZkTransaction::from);
                let l2_stream = self
                    .l2_mempool
                    .best_l2_transactions()
                    .map(ZkTransaction::from);
                Ok(PreparedBlockCommand {
                    block_context,
                    tx_source: Box::pin(l1_stream.chain(l2_stream)),
                    seal_policy: SealPolicy::Decide(deadline, limit),
                    invalid_tx_policy: InvalidTxPolicy::RejectAndContinue,
                    metrics_label: "produce",
                    starting_l1_priority_id,
                })
            }
            BlockCommand::Replay(ReplayRecord {
                block_context,
                starting_l1_priority_id,
                transactions,
            }) => Ok(PreparedBlockCommand {
                block_context,
                seal_policy: SealPolicy::UntilExhausted,
                invalid_tx_policy: InvalidTxPolicy::Abort,
                tx_source: Box::pin(futures::stream::iter(transactions.into_iter())),
                starting_l1_priority_id,
                metrics_label: "replay",
            }),
        }
    }

    pub fn on_canonical_state_change(
        &mut self,
        block_output: &BatchOutput,
        replay_record: &ReplayRecord,
    ) {
        let mut mined_transactions = Vec::new();
        for tx in &replay_record.transactions {
            match tx.envelope() {
                ZkEnvelope::L1(l1_tx) => {
                    self.next_l1_priority_id = l1_tx.priority_id() + 1;
                }
                ZkEnvelope::L2(l2_tx) => {
                    mined_transactions.push(*l2_tx.hash());
                }
            }
        }

        // TODO: confirm whether constructing a real block is absolutely necessary here;
        //       so far it looks like below is sufficient
        let header = Header {
            number: block_output.header.number,
            timestamp: block_output.header.timestamp,
            gas_limit: block_output.header.gas_limit,
            base_fee_per_gas: Some(block_output.header.base_fee_per_gas),
            ..Default::default()
        };
        let body = BlockBody::<L2Envelope>::default();
        let block = Block::new(header, body);
        let sealed_block =
            SealedBlock::new_unchecked(block, BlockHash::from(block_output.header.hash()));
        self.l2_mempool
            .on_canonical_state_change(CanonicalStateUpdate {
                new_tip: &sealed_block,
                pending_block_base_fee: 0,
                pending_block_blob_fee: None,
                // TODO: Pass parsed account property changes here (address, nonce, value)
                changed_accounts: vec![],
                mined_transactions,
                update_kind: PoolUpdateKind::Commit,
            });
    }
}
