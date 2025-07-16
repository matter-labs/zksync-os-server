use crate::model::{
    BlockCommand, InvalidTxPolicy, PreparedBlockCommand, ReplayRecord, SealPolicy,
    UnifiedTransaction,
};
use crate::reth_state::ZkClient;
use alloy::consensus::{Block, BlockBody, Header};
use alloy::primitives::{Address, BlockHash};
use futures::StreamExt;
use reth_execution_types::ChangedAccount;
use reth_primitives::SealedBlock;
use std::collections::HashMap;
use zk_ee::common_structs::PreimageType;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    AccountProperties, ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
};
use zk_os_forward_system::run::BatchOutput;
use zksync_os_mempool::{
    CanonicalStateUpdate, DynL1Pool, L2TransactionPool, PoolUpdateKind, RethPool,
    RethTransactionPoolExt,
};
use zksync_os_types::L2Envelope;

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
                        l1_tx.common_data.serial_id.0 == next_l1_priority_id,
                        "L1 priority ID mismatch: expected {}, got {}",
                        next_l1_priority_id,
                        l1_tx.common_data.serial_id.0
                    );

                    next_l1_priority_id += 1;
                }

                // Create stream: L1 transactions first, then L2 transactions
                let l1_stream = futures::stream::iter(l1_transactions).map(UnifiedTransaction::L1);
                let l2_stream = self
                    .l2_mempool
                    .best_l2_transactions()
                    .map(UnifiedTransaction::L2);
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
                l1_transactions,
                l2_transactions,
            }) => {
                if let Some(first) = l1_transactions.first() {
                    anyhow::ensure!(
                        first.common_data.serial_id.0 == starting_l1_priority_id,
                        "L1 priority ID mismatch: expected {}, got {}",
                        starting_l1_priority_id,
                        first.common_data.serial_id.0
                    )
                }

                let l1_stream = futures::stream::iter(l1_transactions).map(UnifiedTransaction::L1);
                let l2_stream = futures::stream::iter(l2_transactions).map(UnifiedTransaction::L2);

                Ok(PreparedBlockCommand {
                    block_context,
                    seal_policy: SealPolicy::UntilExhausted,
                    invalid_tx_policy: InvalidTxPolicy::Abort,
                    tx_source: Box::pin(l1_stream.chain(l2_stream)),
                    starting_l1_priority_id,
                    metrics_label: "replay",
                })
            }
        }
    }

    pub fn on_canonical_state_change(
        &mut self,
        block_output: &BatchOutput,
        replay_record: &ReplayRecord,
    ) {
        if let Some(last_l1_transaction) = replay_record.l1_transactions.last() {
            self.next_l1_priority_id = last_l1_transaction.common_data.serial_id.0 + 1
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
        let changed_accounts = extract_changed_accounts(block_output);
        self.l2_mempool
            .on_canonical_state_change(CanonicalStateUpdate {
                new_tip: &sealed_block,
                pending_block_base_fee: 0,
                pending_block_blob_fee: None,
                changed_accounts,
                mined_transactions: replay_record
                    .l2_transactions
                    .iter()
                    .map(|tx| *tx.hash())
                    .collect(),
                update_kind: PoolUpdateKind::Commit,
            });
    }
}

/// Extract changed accounts from a BatchOutput.
///
/// This method processes the published preimages and storage writes to extract
/// accounts that were updated during block execution.
pub fn extract_changed_accounts(block_output: &BatchOutput) -> Vec<ChangedAccount> {
    // First, collect all account properties from published preimages
    let mut account_properties_preimages = HashMap::new();
    for (hash, preimage, preimage_type) in &block_output.published_preimages {
        match preimage_type {
            PreimageType::Bytecode => {}
            PreimageType::AccountData => {
                account_properties_preimages.insert(
                    *hash,
                    AccountProperties::decode(
                        &preimage
                            .clone()
                            .try_into()
                            .expect("Preimage should be exactly 124 bytes"),
                    ),
                );
            }
        }
    }

    // Then, map storage writes to account addresses
    let mut result = Vec::new();
    for log in &block_output.storage_writes {
        if log.account == ACCOUNT_PROPERTIES_STORAGE_ADDRESS {
            let account_address = Address::from_slice(&log.account_key.as_u8_array()[12..]);

            if let Some(properties) = account_properties_preimages.get(&log.value) {
                result.push(ChangedAccount {
                    address: account_address,
                    nonce: properties.nonce,
                    balance: properties.balance,
                });
            } else {
                tracing::error!(
                    %account_address,
                    ?log.value,
                    "account properties preimage not found"
                );
                panic!();
            }
        }
    }

    result
}
