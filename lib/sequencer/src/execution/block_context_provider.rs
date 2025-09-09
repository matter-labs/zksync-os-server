use crate::execution::metrics::EXECUTION_METRICS;
use crate::model::blocks::{BlockCommand, InvalidTxPolicy, PreparedBlockCommand, SealPolicy};
use alloy::consensus::{Block, BlockBody, Header};
use alloy::primitives::{Address, BlockHash, TxHash};
use reth_execution_types::ChangedAccount;
use reth_primitives::SealedBlock;
use ruint::aliases::U256;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use zk_os_basic_system::system_implementation::flat_storage_model::{
    ACCOUNT_PROPERTIES_STORAGE_ADDRESS, AccountProperties,
};
use zksync_os_genesis::Genesis;
use zksync_os_interface::types::{BlockContext, BlockHashes, BlockOutput, PreimageType};
use zksync_os_mempool::{
    CanonicalStateUpdate, L2TransactionPool, PoolUpdateKind, ReplayTxStream, best_transactions,
};
use zksync_os_multivm::LATEST_PROTOCOL_VERSION;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{L1PriorityEnvelope, L2Envelope, ZkEnvelope};

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
pub struct BlockContextProvider<Mempool> {
    next_l1_priority_id: u64,
    l1_transactions: mpsc::Receiver<L1PriorityEnvelope>,
    l2_mempool: Mempool,
    block_hashes_for_next_block: BlockHashes,
    previous_block_timestamp: u64,
    chain_id: u64,
    gas_limit: u64,
    pubdata_limit: u64,
    node_version: semver::Version,
    genesis: Genesis,
}

impl<Mempool: L2TransactionPool> BlockContextProvider<Mempool> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        next_l1_priority_id: u64,
        l1_transactions: mpsc::Receiver<L1PriorityEnvelope>,
        l2_mempool: Mempool,
        block_hashes_for_next_block: BlockHashes,
        previous_block_timestamp: u64,
        chain_id: u64,
        gas_limit: u64,
        pubdata_limit: u64,
        node_version: semver::Version,
        genesis: Genesis,
    ) -> Self {
        Self {
            next_l1_priority_id,
            l1_transactions,
            l2_mempool,
            block_hashes_for_next_block,
            previous_block_timestamp,
            chain_id,
            gas_limit,
            pubdata_limit,
            node_version,
            genesis,
        }
    }

    pub async fn prepare_command(
        &mut self,
        block_command: BlockCommand,
    ) -> anyhow::Result<PreparedBlockCommand> {
        let prepared_command = match block_command {
            BlockCommand::Produce(produce_command) => {
                let upgrade_tx = if produce_command.block_number == 1 {
                    Some(self.genesis.genesis_upgrade_tx().await.tx)
                } else {
                    None
                };

                // Create stream:
                // - For block #1 genesis upgrade tx goes first.
                // - L1 transactions first, then L2 transactions.
                let best_txs =
                    best_transactions(&self.l2_mempool, &mut self.l1_transactions, upgrade_tx);
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
                    gas_limit: self.gas_limit,
                    pubdata_limit: self.pubdata_limit,
                    // todo: initialize as source of randomness, i.e. the value of prevRandao
                    mix_hash: Default::default(),
                    protocol_version: LATEST_PROTOCOL_VERSION,
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
                    starting_l1_priority_id: self.next_l1_priority_id,
                    node_version: self.node_version.clone(),
                    expected_block_output_hash: None,
                    previous_block_timestamp: self.previous_block_timestamp,
                }
            }
            BlockCommand::Replay(record) => {
                for tx in &record.transactions {
                    match tx.envelope() {
                        ZkEnvelope::L1(l1_tx) => {
                            assert_eq!(&self.l1_transactions.recv().await.unwrap(), l1_tx);
                        }
                        ZkEnvelope::L2(_) => {}
                        ZkEnvelope::Upgrade(_) => {}
                    }
                }
                PreparedBlockCommand {
                    block_context: record.block_context,
                    seal_policy: SealPolicy::UntilExhausted,
                    invalid_tx_policy: InvalidTxPolicy::Abort,
                    tx_source: Box::pin(ReplayTxStream::new(record.transactions)),
                    starting_l1_priority_id: record.starting_l1_priority_id,
                    metrics_label: "replay",
                    node_version: record.node_version,
                    expected_block_output_hash: Some(record.block_output_hash),
                    previous_block_timestamp: self.previous_block_timestamp,
                }
            }
        };

        Ok(prepared_command)
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
                ZkEnvelope::Upgrade(_) => {}
            }
        }
        EXECUTION_METRICS
            .next_l1_priority_id
            .set(self.next_l1_priority_id);

        // Advance `block_hashes_for_next_block`.
        let last_block_hash = block_output.header.hash();
        self.block_hashes_for_next_block = BlockHashes(
            self.block_hashes_for_next_block
                .0
                .into_iter()
                .skip(1)
                .chain([U256::from_be_bytes(last_block_hash.0)])
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        );
        self.previous_block_timestamp = block_output.header.timestamp;

        // TODO: confirm whether constructing a real block is absolutely necessary here;
        //       so far it looks like below is sufficient
        let header = Header {
            number: block_output.header.number,
            timestamp: block_output.header.timestamp,
            gas_limit: block_output.header.gas_limit,
            base_fee_per_gas: block_output.header.base_fee_per_gas,
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
        if log.account.as_slice()
            == ACCOUNT_PROPERTIES_STORAGE_ADDRESS
                .to_be_bytes::<20>()
                .as_slice()
        {
            let account_address = Address::from_slice(&log.account_key.as_slice()[12..]);

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
