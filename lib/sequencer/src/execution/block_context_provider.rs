use crate::execution::fee_provider::{FeeParams, FeeProvider};
use crate::execution::metrics::EXECUTION_METRICS;
use crate::model::blocks::{BlockCommand, InvalidTxPolicy, PreparedBlockCommand, SealPolicy};
use alloy::primitives::{Address, TxHash, U256};
use anyhow::Context as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{sync::watch, time::Instant};
use zksync_os_interface::types::{BlockContext, BlockHashes, BlockOutput};
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_mempool::{MarkingTxStream, Pool};
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{ExecutionVersion, ProtocolSemanticVersion, ZkEnvelope};

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
pub struct BlockContextProvider<Subpool> {
    next_l1_priority_id: u64,
    next_interop_root_id: u64,
    next_migration_number: u64,
    next_interop_fee_number: u64,
    pool: Pool<Subpool>,
    block_hashes_for_next_block: BlockHashes,
    previous_block_timestamp: u64,
    next_block_number: u64,
    block_time: Duration,
    max_transactions_in_block: usize,
    chain_id: u64,
    gas_limit: u64,
    pubdata_limit: u64,
    interop_roots_per_block: u64,
    service_block_delay: Duration,
    next_interop_tx_allowed_after: Instant,
    /// Protocol version to be used for the next produced block.
    /// Can change in runtime in case of upgrades.
    protocol_version: ProtocolSemanticVersion,
    fee_collector_address: Address,
    last_constructed_block_ctx_sender: watch::Sender<Option<BlockContext>>,
    fee_provider: FeeProvider,
}

impl<Subpool: L2Subpool> BlockContextProvider<Subpool> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        next_l1_priority_id: u64,
        next_interop_root_id: u64,
        next_migration_number: u64,
        next_interop_fee_number: u64,
        pool: Pool<Subpool>,
        block_hashes_for_next_block: BlockHashes,
        previous_block_timestamp: u64,
        next_block_number: u64,
        block_time: Duration,
        max_transactions_in_block: usize,
        chain_id: u64,
        gas_limit: u64,
        pubdata_limit: u64,
        interop_roots_per_block: u64,
        service_block_delay: Duration,
        protocol_version: ProtocolSemanticVersion,
        fee_collector_address: Address,
        last_constructed_block_ctx_sender: watch::Sender<Option<BlockContext>>,
        fee_provider: FeeProvider,
    ) -> Self {
        Self {
            next_l1_priority_id,
            next_interop_root_id,
            next_migration_number,
            next_interop_fee_number,
            pool,
            block_hashes_for_next_block,
            previous_block_timestamp,
            next_block_number,
            block_time,
            max_transactions_in_block,
            chain_id,
            gas_limit,
            pubdata_limit,
            interop_roots_per_block,
            service_block_delay,
            next_interop_tx_allowed_after: Instant::now(),
            protocol_version,
            fee_collector_address,
            last_constructed_block_ctx_sender,
            fee_provider,
        }
    }

    pub async fn prepare_command(
        &mut self,
        block_command: BlockCommand,
    ) -> anyhow::Result<PreparedBlockCommand<'_>> {
        let prepared_command = match block_command {
            BlockCommand::Produce(_) => {
                let fee_params = self.fee_provider.produce_fee_params().await?;
                self.pool
                    .update_pending_block_fees(fee_params.eip1559_basefee.saturating_to(), None);
                let block_number = self.next_block_number;
                // Create stream:
                // - If available, upgrade tx goes first (expected to be the only tx in the block, enforced by sequencer).
                // - L1 transactions first, then L2 transactions.
                let best_txs = self
                    .pool
                    .best_transactions_stream(self.next_interop_tx_allowed_after)
                    .await
                    .context("mempool is closed")?;

                let timestamp = (millis_since_epoch() / 1000) as u64;

                // Check if we peeked an upgrade transaction info.
                // It is possible that we peek an upgrade with version <= self.protocol_version
                // since we do not consume patch upgrades when replaying/rebuilding blocks. Such upgrade can be safely skipped.
                let force_preimages = if let Some(upgrade_metadata) = best_txs.upgrade_metadata
                    && upgrade_metadata.protocol_version > self.protocol_version
                {
                    tracing::info!(
                        block_number,
                        ?upgrade_metadata,
                        "including protocol upgrade transaction in the block"
                    );
                    // Invariant: transactions sent through this stream must be ready for execution, e.g.
                    // transaction should not be sent until timestamp is reached.
                    // We add some margin of error for timestamp comparison.
                    let current_timestamp = timestamp.saturating_add(5);
                    anyhow::ensure!(
                        upgrade_metadata.timestamp <= current_timestamp,
                        "upgrade transaction with timestamp {} received too early at {}; tx: {upgrade_metadata:?}",
                        upgrade_metadata.timestamp,
                        current_timestamp
                    );
                    self.protocol_version = upgrade_metadata.protocol_version.clone();
                    upgrade_metadata.force_preimages.clone()
                } else {
                    Vec::new()
                };

                let execution_version: ExecutionVersion = (&self.protocol_version)
                    .try_into()
                    .context("Cannot instantiate a block for unsupported execution version")?;

                let FeeParams {
                    eip1559_basefee,
                    native_price,
                    pubdata_price,
                } = fee_params;
                let block_context = BlockContext {
                    eip1559_basefee,
                    native_price,
                    pubdata_price,
                    block_number,
                    timestamp,
                    chain_id: self.chain_id,
                    coinbase: self.fee_collector_address,
                    block_hashes: self.block_hashes_for_next_block,
                    gas_limit: self.gas_limit,
                    pubdata_limit: self.pubdata_limit,
                    // todo: initialize as source of randomness, i.e. the value of prevRandao
                    mix_hash: Default::default(),
                    execution_version: execution_version as u32,
                    blob_fee: U256::ONE,
                };
                self.last_constructed_block_ctx_sender
                    .send_replace(Some(block_context));
                PreparedBlockCommand {
                    block_context,
                    tx_source: best_txs.stream,
                    seal_policy: SealPolicy::Decide(
                        self.block_time,
                        self.max_transactions_in_block,
                    ),
                    invalid_tx_policy: InvalidTxPolicy::RejectAndContinue,
                    metrics_label: "produce",
                    starting_l1_priority_id: self.next_l1_priority_id,
                    protocol_version: self.protocol_version.clone(),
                    expected_block_output_hash: None,
                    previous_block_timestamp: self.previous_block_timestamp,
                    force_preimages,
                    starting_interop_root_id: self.next_interop_root_id,
                    starting_migration_number: self.next_migration_number,
                    starting_interop_fee_number: self.next_interop_fee_number,
                    interop_roots_per_block: self.interop_roots_per_block,
                    strict_subpool_cleanup: true,
                }
            }
            BlockCommand::Replay(record) => {
                anyhow::ensure!(
                    self.next_block_number == record.block_context.block_number,
                    "blocks received our of order: {} in component state, {} in resolved ReplayRecord",
                    self.next_block_number,
                    record.block_context.block_number
                );
                anyhow::ensure!(
                    self.previous_block_timestamp == record.previous_block_timestamp,
                    "inconsistent previous block timestamp: {} in component state, {} in resolved ReplayRecord",
                    self.previous_block_timestamp,
                    record.previous_block_timestamp
                );
                anyhow::ensure!(
                    self.block_hashes_for_next_block == record.block_context.block_hashes,
                    "inconsistent previous block hashes: {} in component state, {} in resolved ReplayRecord",
                    self.previous_block_timestamp,
                    record.previous_block_timestamp
                );
                PreparedBlockCommand {
                    block_context: record.block_context,
                    seal_policy: SealPolicy::UntilExhausted {
                        allowed_to_finish_early: false,
                    },
                    invalid_tx_policy: InvalidTxPolicy::Abort,
                    tx_source: MarkingTxStream::unmarkable(futures::stream::iter(
                        record.transactions,
                    )),
                    starting_l1_priority_id: record.starting_l1_priority_id,
                    metrics_label: "replay",
                    protocol_version: record.protocol_version,
                    expected_block_output_hash: Some(record.block_output_hash),
                    previous_block_timestamp: self.previous_block_timestamp,
                    force_preimages: record.force_preimages,
                    starting_interop_root_id: record.starting_interop_root_id,
                    starting_migration_number: record.starting_migration_number,
                    starting_interop_fee_number: record.starting_interop_fee_number,
                    interop_roots_per_block: self.interop_roots_per_block,
                    strict_subpool_cleanup: false,
                }
            }
            BlockCommand::Rebuild(rebuild) => {
                let block_number = rebuild.replay_record.block_context.block_number;
                let (execution_version, protocol_version) = (
                    rebuild.replay_record.block_context.execution_version,
                    rebuild.replay_record.protocol_version,
                );

                if rebuild.make_empty
                    && rebuild
                        .replay_record
                        .transactions
                        .iter()
                        .any(|tx| matches!(tx.envelope(), ZkEnvelope::Upgrade(_)))
                {
                    anyhow::bail!(
                        "Cannot make an empty block when there is an upgrade transaction in the replay record for block {}",
                        block_number
                    );
                }

                let block_context = BlockContext {
                    eip1559_basefee: rebuild.replay_record.block_context.eip1559_basefee,
                    native_price: rebuild.replay_record.block_context.native_price,
                    pubdata_price: rebuild.replay_record.block_context.pubdata_price,
                    block_number,
                    timestamp: rebuild.replay_record.block_context.timestamp,
                    blob_fee: rebuild.replay_record.block_context.blob_fee,
                    chain_id: self.chain_id,
                    coinbase: self.fee_collector_address,
                    block_hashes: self.block_hashes_for_next_block,
                    gas_limit: self.gas_limit,
                    pubdata_limit: self.pubdata_limit,
                    // todo: initialize as source of randomness, i.e. the value of prevRandao
                    mix_hash: Default::default(),
                    execution_version,
                };
                let txs = if rebuild.make_empty {
                    Vec::new()
                } else {
                    let first_l1_tx = rebuild
                        .replay_record
                        .transactions
                        .iter()
                        .find(|tx| matches!(tx.envelope(), ZkEnvelope::L1(_)));
                    // It's possible that we haven't processed some L1 transaction from previous blocks when rebuilding.
                    // In that case we shouldn't consider next L1 txs when rebuilding.
                    let filter_l1_txs =
                        if let Some(ZkEnvelope::L1(l1_tx)) = first_l1_tx.map(|tx| tx.envelope()) {
                            l1_tx.priority_id() != self.next_l1_priority_id
                        } else {
                            false
                        };
                    if filter_l1_txs {
                        rebuild
                            .replay_record
                            .transactions
                            .into_iter()
                            .filter(|tx| !matches!(tx.envelope(), ZkEnvelope::L1(_)))
                            .collect()
                    } else {
                        rebuild.replay_record.transactions
                    }
                };

                PreparedBlockCommand {
                    block_context,
                    tx_source: MarkingTxStream::unmarkable(futures::stream::iter(txs)),
                    seal_policy: SealPolicy::UntilExhausted {
                        allowed_to_finish_early: true,
                    },
                    invalid_tx_policy: InvalidTxPolicy::RejectAndContinue,
                    metrics_label: "rebuild",
                    starting_l1_priority_id: self.next_l1_priority_id,
                    protocol_version,
                    expected_block_output_hash: None,
                    previous_block_timestamp: self.previous_block_timestamp,
                    force_preimages: rebuild.replay_record.force_preimages,
                    starting_interop_root_id: self.next_interop_root_id,
                    starting_migration_number: self.next_migration_number,
                    starting_interop_fee_number: self.next_interop_fee_number,
                    interop_roots_per_block: self.interop_roots_per_block,
                    strict_subpool_cleanup: false,
                }
            }
        };

        Ok(prepared_command)
    }

    pub fn remove_transactions(&self, tx_hashes: Vec<TxHash>) {
        self.pool.remove_transactions(tx_hashes);
    }

    pub async fn on_canonical_state_change(
        &mut self,
        block_output: &BlockOutput,
        replay_record: &ReplayRecord,
        strict_subpool_cleanup: bool,
    ) {
        let outcome = self
            .pool
            .on_canonical_state_change(
                block_output.header.clone(),
                &block_output.account_diffs,
                replay_record,
                strict_subpool_cleanup,
            )
            .await;
        if let Some(last_l1_priority_id) = outcome.last_l1_priority_id {
            self.next_l1_priority_id = last_l1_priority_id + 1;
            EXECUTION_METRICS
                .next_l1_priority_id
                .set(self.next_l1_priority_id);
        }
        if let Some(last_interop_log_id) = outcome.last_interop_log_index {
            self.next_interop_tx_allowed_after = Instant::now() + self.service_block_delay;
            self.next_interop_root_id = last_interop_log_id + 1;
        }

        if let Some(last_migration_number) = outcome.last_migration_number {
            self.next_migration_number = last_migration_number + 1;
        }
        if let Some(last_interop_fee_number) = outcome.last_interop_fee_number {
            self.next_interop_fee_number = last_interop_fee_number + 1;
        }

        // We update protocol version here, so that we take into account replay records with protocol version bumps.
        self.protocol_version = replay_record.protocol_version.clone();

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
        self.next_block_number += 1;
        self.previous_block_timestamp = block_output.header.timestamp;
        self.fee_provider.on_canonical_state_change(replay_record);
    }
}

pub fn millis_since_epoch() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Incorrect system time")
        .as_millis()
}
