use crate::metrics::TRANSACTION_POOL_METRICS;
use crate::subpools::interop_fee::InteropFeeSubpool;
use crate::subpools::interop_roots::InteropRootsSubpool;
use crate::subpools::l1::L1Subpool;
use crate::subpools::l2::{L2Subpool, L2TransactionsStreamMarker};
use crate::subpools::sl_chain_id::SlChainIdSubpool;
use crate::subpools::upgrade::{UpgradeSubpool, UpgradeTransactionsStream};
use alloy::consensus::{Header, Sealed};
use alloy::primitives::TxHash;
use futures::stream::{BoxStream, PollNext};
use futures::{Stream, StreamExt};
use reth_execution_types::ChangedAccount;
use reth_primitives::{Block, BlockBody};
use reth_primitives_traits::SealedBlock;
use reth_transaction_pool::{CanonicalStateUpdate, PoolUpdateKind};
use tokio::time::Instant;
use zksync_os_interface::types::AccountDiff;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{L1TxSerialId, SystemTxType, UpgradeMetadata, ZkEnvelope, ZkTransaction};

/// General pool that provides unified access to all transaction sources in the system.
///
/// Consists of multiple smaller subpools, see [`crate::subpools`] for more information.
pub struct Pool<T> {
    upgrade_subpool: UpgradeSubpool,
    sl_chain_id_subpool: SlChainIdSubpool,
    interop_fee_subpool: InteropFeeSubpool,
    interop_roots_subpool: InteropRootsSubpool,
    l1_subpool: L1Subpool,
    l2_subpool: T,
}

impl<T: L2Subpool> Pool<T> {
    pub fn new(
        upgrade_subpool: UpgradeSubpool,
        sl_chain_id_subpool: SlChainIdSubpool,
        interop_fee_subpool: InteropFeeSubpool,
        interop_roots_subpool: InteropRootsSubpool,
        l1_subpool: L1Subpool,
        l2_subpool: T,
    ) -> Self {
        Self {
            upgrade_subpool,
            sl_chain_id_subpool,
            interop_fee_subpool,
            interop_roots_subpool,
            l1_subpool,
            l2_subpool,
        }
    }

    /// Picks the best source of transactions out of currently available ones. If there are none,
    /// then waits for one to become available.
    ///
    /// Also provides upgrade information is there is one (which is not necessarily accompanied by
    /// an upgrade transaction).
    ///
    /// Returns `None` if all transaction sources are closed.
    pub async fn best_transactions_stream<'a>(
        &'a mut self,
        next_interop_tx_allowed_after: Instant,
    ) -> Option<StreamOutcome<'a>> {
        let mut upgrade_info_stream = self.upgrade_subpool.upgrade_info_stream().await;

        let interop_root_stream = tokio_stream::StreamExt::peekable(
            self.interop_roots_subpool
                .interop_transactions_with_delay(next_interop_tx_allowed_after)
                .await,
        );

        let mut sl_chain_id_stream = tokio_stream::StreamExt::peekable(
            self.sl_chain_id_subpool.best_transactions_stream().await,
        );
        let interop_fee_stream = tokio_stream::StreamExt::peekable(
            self.interop_fee_subpool.best_transactions_stream().await,
        );

        let l1_stream = self.l1_subpool.best_transactions_stream().await;
        let l2_stream = self.l2_subpool.best_transactions_stream();
        let l2_marker = l2_stream.marker();
        fn prio_left(_: &mut ()) -> PollNext {
            PollNext::Left
        }
        let l1_l2_stream = futures::stream::select_with_strategy(l1_stream, l2_stream, prio_left);
        let mut l1_l2_stream = tokio_stream::StreamExt::peekable(l1_l2_stream);

        let interop_related_stream = futures::stream::select_with_strategy(
            interop_fee_stream,
            interop_root_stream,
            prio_left,
        );
        let mut interop_related_stream = tokio_stream::StreamExt::peekable(interop_related_stream);

        let mut upgrade_metadata = None;
        loop {
            tokio::select! {
                // This select is biased on purpose, meaning `tokio::select!` branches are checked
                // sequentially top to bottom. Transaction types must be ordered by priority -
                // otherwise, if there is some frequent transaction type in the top, under load
                // we might never poll and pick a rarer but important transaction type.
                biased;

                // Upgrade branch is a bit special as it does not always produce a stream of
                // transactions. Sometimes it only sets `upgrade_metadata` and some other stream
                // needs to provide transactions. This is the reason behind `loop` above (which can
                // iterate twice at max).
                Some(upgrade) = tokio_stream::StreamExt::next(&mut upgrade_info_stream) => {
                    if let Some(upgrade_tx) = &upgrade.tx {
                        tracing::info!(
                            protocol_version = %upgrade.metadata.protocol_version,
                            tx_hash = %upgrade_tx.hash(),
                            "L1 upgrade transaction found for protocol version {}",
                            upgrade.metadata.protocol_version,
                        )
                    } else {
                        tracing::info!(
                            protocol_version = %upgrade.metadata.protocol_version,
                            "L1 patch upgrade (no tx) found for protocol version {}",
                            upgrade.metadata.protocol_version,
                        )
                    }
                    upgrade_metadata = Some(upgrade.metadata);
                    if let Some(tx) = upgrade.tx {
                        return Some(StreamOutcome {
                            upgrade_metadata,
                            stream: MarkingTxStream::unmarkable(UpgradeTransactionsStream::one(tx)),
                        });
                    }
                }
                Some(_) = sl_chain_id_stream.peek() => {
                    // todo: this will make sure that SL chain ID transaction is in its own block.
                    //       But we only need to ensure that, if present, it is the first transaction
                    //       in the block. In other words, we could chain it with `l1_l2_stream` as
                    //       a micro-optimization. Given how rare it is, likely not worth the trouble.
                    return Some(StreamOutcome {
                        upgrade_metadata,
                        stream: MarkingTxStream::unmarkable(sl_chain_id_stream),
                    });
                }
                Some(_) = interop_related_stream.peek() => {
                    return Some(StreamOutcome {
                        upgrade_metadata,
                        stream: MarkingTxStream::unmarkable(interop_related_stream),
                    });
                }
                Some(_) = l1_l2_stream.peek() => {
                    return Some(StreamOutcome {
                        upgrade_metadata,
                        stream: MarkingTxStream::markable(l1_l2_stream, l2_marker),
                    });
                }

                else => {
                    return None;
                }
            }
        }
    }

    /// Removes transactions from the local pool when forwarding to the main node fails after
    /// local insertion. Records them in the `forwarding_rollback_transactions` metric.
    pub fn remove_transactions(&self, tx_hashes: Vec<TxHash>) {
        TRANSACTION_POOL_METRICS
            .forwarding_rollback_transactions
            .inc_by(tx_hashes.len() as u64);
        self.l2_subpool.remove_transactions(tx_hashes);
    }

    /// Removes transactions that were rejected by the ZK VM during block execution and
    /// records them in the `purged_transactions` metric.
    pub fn purge_transactions(&self, tx_hashes: Vec<TxHash>) {
        TRANSACTION_POOL_METRICS
            .purged_transactions
            .inc_by(tx_hashes.len() as u64);
        self.l2_subpool.remove_transactions(tx_hashes);
    }

    pub fn update_pending_block_fees(
        &self,
        pending_block_base_fee: u64,
        pending_block_blob_fee: Option<u128>,
    ) {
        let mut block_info = self.l2_subpool.block_info();
        block_info.pending_basefee = pending_block_base_fee;
        block_info.pending_blob_fee = pending_block_blob_fee;
        self.l2_subpool.set_block_info(block_info);
    }

    pub async fn on_canonical_state_change(
        &self,
        header: Sealed<Header>,
        account_diffs: &[AccountDiff],
        replay_record: &ReplayRecord,
        strict_subpool_cleanup: bool,
    ) -> StateChangeOutcome {
        let mut upgrade_txs = Vec::new();
        let mut interop_txs = Vec::new();
        let mut interop_fee_txs = Vec::new();
        let mut sl_chain_id_txs = Vec::new();
        let mut l1_transactions = Vec::new();
        let mut l2_transactions = Vec::new();
        for tx in &replay_record.transactions {
            match tx.envelope() {
                ZkEnvelope::System(system_tx) => match system_tx.system_subtype() {
                    SystemTxType::ImportInteropRoots(_) => {
                        interop_txs.push(system_tx);
                    }
                    SystemTxType::SetInteropFee(_) => {
                        interop_fee_txs.push(system_tx);
                    }
                    SystemTxType::SetSLChainId(_) => {
                        sl_chain_id_txs.push(system_tx);
                    }
                },
                ZkEnvelope::L1(l1_tx) => {
                    l1_transactions.push(l1_tx);
                }
                ZkEnvelope::L2(l2_tx) => {
                    l2_transactions.push(*l2_tx.hash());
                }
                ZkEnvelope::Upgrade(upgrade) => {
                    upgrade_txs.push(upgrade);
                }
            }
        }
        self.upgrade_subpool
            .on_canonical_state_change(&replay_record.protocol_version, upgrade_txs)
            .await;
        let last_interop_log_id = self
            .interop_roots_subpool
            .on_canonical_state_change(interop_txs)
            .await;
        let last_interop_fee_number = self
            .interop_fee_subpool
            .on_canonical_state_change(interop_fee_txs, strict_subpool_cleanup)
            .await;
        let last_migration_number = self
            .sl_chain_id_subpool
            .on_canonical_state_change(sl_chain_id_txs)
            .await;
        let last_l1_priority_id = self
            .l1_subpool
            .on_canonical_state_change(l1_transactions)
            .await;

        let (header, hash) = header.into_parts();
        let body = BlockBody::default();
        let block = Block::new(header, body);
        let sealed_block = SealedBlock::new_unchecked(block, hash);
        let changed_accounts = account_diffs
            .iter()
            .map(|diff| ChangedAccount {
                address: diff.address,
                nonce: diff.nonce,
                balance: diff.balance,
            })
            .collect();
        self.l2_subpool
            .on_canonical_state_change(CanonicalStateUpdate {
                new_tip: &sealed_block,
                // pending block fees will be set later in `update_pending_block_fees`
                pending_block_base_fee: 0,
                pending_block_blob_fee: None,
                changed_accounts,
                mined_transactions: l2_transactions,
                update_kind: PoolUpdateKind::Commit,
            });

        StateChangeOutcome {
            last_interop_log_id,
            last_l1_priority_id,
            last_migration_number,
            last_interop_fee_number,
        }
    }
}

pub struct StreamOutcome<'a> {
    /// Optional upgrade metadata to be applied with transactions in `stream`. Note that even if
    /// this is `Some`, `stream` is not guaranteed to contain an upgrade transaction. The stream may
    /// contain other transaction types if the upgrade is a patch upgrade.
    pub upgrade_metadata: Option<UpgradeMetadata>,
    /// Non-empty stream of transactions.
    pub stream: MarkingTxStream<'a>,
}

#[derive(Debug, Default)]
pub struct StateChangeOutcome {
    /// Last interop log_id that was imported after canonical state change.
    pub last_interop_log_id: Option<u64>,
    /// Last L1 priority ID that was executed after canonical state change.
    pub last_l1_priority_id: Option<L1TxSerialId>,
    /// Last migration number that was executed after canonical state change.
    pub last_migration_number: Option<u64>,
    /// Last interop fee update number that was executed after canonical state change.
    pub last_interop_fee_number: Option<u64>,
}

/// Transaction stream that is capable of marking last L2 transaction as invalid.
pub struct MarkingTxStream<'a> {
    pub stream: BoxStream<'a, ZkTransaction>,
    marker: Option<L2TransactionsStreamMarker>,
}

impl<'a> MarkingTxStream<'a> {
    pub fn unmarkable(stream: impl Stream<Item = ZkTransaction> + Send + 'a) -> Self {
        Self {
            stream: stream.boxed(),
            marker: None,
        }
    }

    fn markable(
        stream: impl Stream<Item = ZkTransaction> + Send + 'a,
        marker: L2TransactionsStreamMarker,
    ) -> Self {
        Self {
            stream: stream.boxed(),
            marker: Some(marker),
        }
    }

    pub fn mark_last_l2_tx_as_invalid(&self) {
        let Some(marker) = self.marker.as_ref() else {
            panic!(
                "tried to mark last L2 transaction as invalid but this stream does not serve L2 transactions"
            )
        };
        marker.mark_last_tx_as_invalid()
    }
}
