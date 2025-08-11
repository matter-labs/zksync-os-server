use crate::execution::block_executor::execute_block;
use crate::execution::metrics::SequencerState;
use crate::model::blocks::{BlockCommand, InvalidTxPolicy, PreparedBlockCommand, SealPolicy};
use crate::reth_state::ZkClient;
use alloy::consensus::{Block, BlockBody, Header};
use alloy::primitives::{Address, BlockHash, TxHash};
use anyhow::Context;
use reth_execution_types::ChangedAccount;
use reth_primitives::SealedBlock;
use ruint::aliases::U256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use zk_ee::common_structs::PreimageType;
use zk_ee::system::metadata::BlockHashes;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties,
};
use zk_os_forward_system::run::{BlockContext, BlockOutput, InvalidTransaction};
use zksync_os_l1_watcher::L1_METRICS;
use zksync_os_mempool::{
    CanonicalStateUpdate, PoolUpdateKind, ReplayTxStream, RethPool, RethTransactionPool,
    RethTransactionPoolExt, best_transactions,
};
use zksync_os_observability::{ComponentStateLatencyTracker, LatencyDistributionTracker};
use zksync_os_state::StateHandle;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{L1Envelope, L2Envelope, ZkEnvelope};

/// Component that turns `BlockCommand`s into `PreparedBlockCommand`s.
/// Last step in the stream where `Produce` and `Replay` are differentiated.
///
///  * Tracks L1 priority ID and 256 previous block hashes.
///  * Combines the L1 and L2 transactions
///  * Cross-checks L1 transactions in Replay blocks against L1 (important for ENs) todo: not implemented yet
///
/// Note: unlike other components, this one doesn't tolerate replaying blocks -
///  it doesn't tolerate jumps in L1 priority IDs.
///  this is easily fixable if needed.
pub struct BlockContextProvider {
    next_l1_priority_id: u64,
    l1_transactions: mpsc::Receiver<L1Envelope>,
    l2_mempool: RethPool<ZkClient>,
    block_hashes_for_next_block: BlockHashes,
    chain_id: u64,
}

impl BlockContextProvider {
    pub fn new(
        next_l1_priority_id: u64,
        l1_transactions: mpsc::Receiver<L1Envelope>,
        l2_mempool: RethPool<ZkClient>,
        block_hashes_for_next_block: BlockHashes,
        chain_id: u64,
    ) -> Self {
        Self {
            next_l1_priority_id,
            l1_transactions,
            l2_mempool,
            block_hashes_for_next_block,
            chain_id,
        }
    }

    pub async fn execute_block(
        &mut self,
        block_command: BlockCommand,
        state: StateHandle,
        latency_tracker: ComponentStateLatencyTracker<SequencerState>,
    ) -> anyhow::Result<(BlockOutput, ReplayRecord, Vec<(TxHash, InvalidTransaction)>)> {
        latency_tracker.set_state(SequencerState::PreparingBlockCommand);
        let block_number = block_command.block_number();
        tracing::debug!(
            block_number,
            cmd = block_command.to_string(),
            "▶ starting command. Turning into PreparedCommand.."
        );

        // todo: validate next_l1_transaction_id by adding it directly to BlockCommand
        //  it's not clear whether we want to add it only to Replay or also in Produce

        let prepared_command = match block_command {
            BlockCommand::Produce(produce_command) => {
                let starting_l1_priority_id = self.next_l1_priority_id;

                // TODO: should drop the l1 tx that come before starting_l1_priority_id

                // Create stream: L1 transactions first, then L2 transactions
                let best_txs = best_transactions(&self.l2_mempool, &mut self.l1_transactions);
                let gas_limit = 100_000_000;
                let pubdata_limit = 100_000_000;
                let timestamp = (millis_since_epoch() / 1000) as u64;
                let block_context = BlockContext {
                    eip1559_basefee: U256::from(1000),
                    native_price: U256::from(1),
                    gas_per_pubdata: Default::default(),
                    block_number: produce_command.block_number,
                    timestamp,
                    chain_id: self.chain_id,
                    coinbase: Default::default(),
                    block_hashes: self.block_hashes_for_next_block,
                    gas_limit,
                    pubdata_limit,
                    // todo: initialize as source of randomness, i.e. the value of prevRandao
                    mix_hash: Default::default(),
                };
                PreparedBlockCommand {
                    block_context,
                    tx_source: Box::pin(best_txs),
                    seal_policy: SealPolicy::Decide(
                        produce_command.block_time,
                        produce_command.max_transactions_in_block,
                    ),
                    invalid_tx_policy: InvalidTxPolicy::RejectAndContinue,
                    metrics_label: "produce",
                    starting_l1_priority_id,
                }
            }
            BlockCommand::Replay(record) => {
                for tx in &record.transactions {
                    match tx.envelope() {
                        ZkEnvelope::L1(l1_tx) => {
                            assert_eq!(
                                self.l1_transactions.recv().await.unwrap().priority_id(),
                                l1_tx.priority_id()
                            );
                        }
                        ZkEnvelope::L2(_) => {} // already consumed l1 transactions in execution},
                    }
                }
                PreparedBlockCommand {
                    block_context: record.block_context,
                    seal_policy: SealPolicy::UntilExhausted,
                    invalid_tx_policy: InvalidTxPolicy::Abort,
                    tx_source: Box::pin(ReplayTxStream::new(record.transactions)),
                    starting_l1_priority_id: record.starting_l1_priority_id,
                    metrics_label: "replay",
                }
            }
        };

        tracing::debug!(
            block_number,
            starting_l1_priority_id = prepared_command.starting_l1_priority_id,
            "Prepared command. Executing...",
        );
        latency_tracker.set_state(SequencerState::PreparingBlockCommand);

        let (block_output, replay_record, purged_txs) =
            execute_block(prepared_command, state, latency_tracker.clone())
                .await
                .context("execute_block")?;

        Ok((block_output, replay_record, purged_txs))
    }

    pub fn remove_txs(&self, tx_hashes: Vec<TxHash>) {
        self.l2_mempool.remove_transactions(tx_hashes);
    }

    pub fn on_canonical_state_change(
        &mut self,
        block_output: &BlockOutput,
        replay_record: &ReplayRecord,
    ) {
        let mut l2_transactions = Vec::new();
        for tx in &replay_record.transactions {
            match tx.envelope() {
                ZkEnvelope::L1(l1_tx) => {
                    self.next_l1_priority_id = l1_tx.priority_id() + 1;
                }
                ZkEnvelope::L2(l2_tx) => {
                    l2_transactions.push(*l2_tx.hash());
                }
            }
        }
        L1_METRICS.next_l1_priority_id.set(self.next_l1_priority_id);

        // Advance `block_hashes_for_next_block`.
        let last_block_hash = block_output.header.hash();
        self.block_hashes_for_next_block = BlockHashes(
            self.block_hashes_for_next_block
                .0
                .into_iter()
                .skip(1)
                .chain([U256::from_be_bytes(last_block_hash)])
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        );

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
                mined_transactions: l2_transactions,
                update_kind: PoolUpdateKind::Commit,
            });
    }
}

/// Extract changed accounts from a BlockOutput.
///
/// This method processes the published preimages and storage writes to extract
/// accounts that were updated during block execution.
pub fn extract_changed_accounts(block_output: &BlockOutput) -> Vec<ChangedAccount> {
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

pub fn millis_since_epoch() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Incorrect system time")
        .as_millis()
}
