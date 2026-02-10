use crate::subpools::interop_roots::InteropRootsSubpool;
use crate::subpools::l1::{L1Subpool, L1TransactionsStream};
use crate::subpools::l2::{L2Subpool, L2TransactionsStream};
use crate::subpools::sl_chain_id::SlChainIdSubpool;
use crate::subpools::upgrade::{UpgradeSubpool, UpgradeTransactionsStream};
use alloy::consensus::{Block, BlockBody, Header, Sealed};
use alloy::primitives::TxHash;
use futures::Stream;
use pin_project::pin_project;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::SealedBlock;
use reth_transaction_pool::{CanonicalStateUpdate, PoolUpdateKind};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::time::Instant;
use tokio_stream::StreamExt;
use zksync_os_interface::types::AccountDiff;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{
    InteropRootsLogIndex, L1TxSerialId, L2Envelope, SystemTxType, UpgradeMetadata, ZkEnvelope,
    ZkTransaction,
};

pub struct Pool<T> {
    upgrade_subpool: UpgradeSubpool,
    sl_chain_id_subpool: SlChainIdSubpool,
    interop_roots_subpool: InteropRootsSubpool,
    l1_subpool: L1Subpool,
    l2_subpool: T,
}

impl<T: L2Subpool> Pool<T> {
    pub fn new(
        upgrade_subpool: UpgradeSubpool,
        sl_chain_id_subpool: SlChainIdSubpool,
        interop_roots_subpool: InteropRootsSubpool,
        l1_subpool: L1Subpool,
        l2_subpool: T,
    ) -> Self {
        Self {
            upgrade_subpool,
            sl_chain_id_subpool,
            interop_roots_subpool,
            l1_subpool,
            l2_subpool,
        }
    }

    pub async fn best_transactions_stream<'a>(
        &'a mut self,
        next_interop_tx_allowed_after: Instant,
    ) -> StreamOutcome<'a> {
        let mut upgrade_info_stream = self.upgrade_subpool.upgrade_info_stream();

        let interop_stream = self
            .interop_roots_subpool
            .interop_transactions_with_delay(next_interop_tx_allowed_after);
        let mut interop_stream = crate::peekable::Peekable::new(interop_stream);

        let sl_chain_id_stream = self.sl_chain_id_subpool.best_transactions_stream();
        let mut sl_chain_id_stream = crate::peekable::Peekable::new(sl_chain_id_stream);

        let l1_stream = self.l1_subpool.best_transactions_stream();
        let l2_stream = self.l2_subpool.best_transactions_stream();
        let l1_l2_stream = L1L2TxStream {
            last_polled_tx: None,
            l1_stream,
            l2_stream,
        };
        let mut l1_l2_stream = crate::peekable::Peekable::new(l1_l2_stream);

        let mut upgrade_metadata = None;
        loop {
            tokio::select! {
                // If you run this example without `biased;`, the polling order is
                // pseudo-random, and the assertions on the value of count will
                // (probably) fail.
                biased;

                Some(upgrade) = upgrade_info_stream.next() => {
                    upgrade_metadata = Some(upgrade.metadata);
                    if let Some(tx) = upgrade.tx {
                        return StreamOutcome {
                            upgrade_metadata,
                            stream: UpgradeTransactionsStream::one(tx).boxed_tx_stream(),
                        }
                    }
                }
                Some(_) = sl_chain_id_stream.peek() => {
                    // todo: chain with `l1_l2_stream`
                    return StreamOutcome {
                        upgrade_metadata,
                        stream: sl_chain_id_stream.boxed_tx_stream(),
                    }
                }
                Some(_) = interop_stream.peek() => {
                    return StreamOutcome {
                        upgrade_metadata,
                        stream: interop_stream.boxed_tx_stream(),
                    }
                }
                Some(_) = l1_l2_stream.peek() => {
                    return StreamOutcome {
                        upgrade_metadata,
                        stream: l1_l2_stream.boxed_tx_stream(),
                    }
                }

                else => {
                    todo!()
                }
            }
        }
    }

    pub fn remove_transactions(&self, tx_hashes: Vec<TxHash>) {
        self.l2_subpool.remove_transactions(tx_hashes);
    }

    pub async fn on_canonical_state_change(
        &self,
        header: Sealed<Header>,
        account_diffs: &[AccountDiff],
        replay_record: &ReplayRecord,
    ) -> StateChangeOutcome {
        let mut upgrade_txs = Vec::new();
        let mut interop_txs = Vec::new();
        let mut sl_chain_id_txs = Vec::new();
        let mut l1_transactions = Vec::new();
        let mut l2_transactions = Vec::new();
        for tx in &replay_record.transactions {
            match tx.envelope() {
                ZkEnvelope::System(system_tx) => match system_tx.system_subtype() {
                    SystemTxType::ImportInteropRoots(_) => {
                        interop_txs.push(system_tx);
                    }
                    SystemTxType::SetSLChainId => {
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
        let last_interop_log_index = self
            .interop_roots_subpool
            .on_canonical_state_change(interop_txs);
        self.sl_chain_id_subpool
            .on_canonical_state_change(sl_chain_id_txs)
            .await;
        let last_l1_priority_id = self
            .l1_subpool
            .on_canonical_state_change(l1_transactions)
            .await;

        let (header, hash) = header.into_parts();
        let body = BlockBody::<L2Envelope>::default();
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
                pending_block_base_fee: 0,
                pending_block_blob_fee: None,
                changed_accounts,
                mined_transactions: l2_transactions,
                update_kind: PoolUpdateKind::Commit,
            });

        StateChangeOutcome {
            last_interop_log_index,
            last_l1_priority_id,
        }
    }
}

#[derive(Debug, Default)]
pub struct StateChangeOutcome {
    pub last_interop_log_index: Option<InteropRootsLogIndex>,
    pub last_l1_priority_id: Option<L1TxSerialId>,
}

pub trait TxStream: Stream<Item = ZkTransaction> {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>);

    fn boxed_tx_stream<'a>(self) -> BoxTxStream<'a>
    where
        Self: Sized + Send + 'a,
    {
        Box::pin(self)
    }
}

pub type BoxTxStream<'a> = Pin<Box<dyn TxStream + Send + 'a>>;

impl<S: TxStream> TxStream for crate::peekable::Peekable<S> {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>) {
        self.project().stream.mark_last_tx_as_invalid()
    }
}

#[pin_project]
pub struct L1L2TxStream {
    last_polled_tx: Option<LastPolledType>,
    #[pin]
    l1_stream: L1TransactionsStream,
    #[pin]
    l2_stream: L2TransactionsStream,
}

enum LastPolledType {
    L1,
    L2,
}

impl Stream for L1L2TxStream {
    type Item = ZkTransaction;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        if let Poll::Ready(tx) = this.l1_stream.poll_next(cx) {
            *this.last_polled_tx = Some(LastPolledType::L1);
            // We propagate `None` here on purpose. L1 stream closing is end-of-life scenario and
            // this stream should be closed too instead of defaulting to `l2`.
            return Poll::Ready(tx);
        }
        if let Poll::Ready(tx) = this.l2_stream.poll_next(cx) {
            *this.last_polled_tx = Some(LastPolledType::L2);
            // Same things here - we propagate `None` on purpose
            return Poll::Ready(tx);
        }
        Poll::Pending
    }
}

impl TxStream for L1L2TxStream {
    fn mark_last_tx_as_invalid(mut self: Pin<&mut Self>) {
        match self.last_polled_tx {
            None => {
                tracing::error!("tried to mark non-existing transaction as invalid")
            }
            Some(LastPolledType::L1) => self.as_mut().project().l1_stream.mark_last_tx_as_invalid(),
            Some(LastPolledType::L2) => self.as_mut().project().l2_stream.mark_last_tx_as_invalid(),
        }
    }
}

pub struct StreamOutcome<'a> {
    /// Optional upgrade metadata to be applied with transactions in `stream`. Note that even if
    /// this is `Some`, `stream` is not guaranteed to contain an upgrade transaction. The stream may
    /// contain other transaction types if the upgrade is a patch upgrade.
    pub upgrade_metadata: Option<UpgradeMetadata>,
    /// Non-empty stream of transactions.
    pub stream: BoxTxStream<'a>,
}
