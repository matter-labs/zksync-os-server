use crate::subpools::interop_roots::InteropRootsSubpool;
use crate::subpools::l1::{L1Subpool, L1TransactionsStream};
use crate::subpools::l2::{L2Subpool, L2TransactionsStream};
use alloy::consensus::{Block, BlockBody, Header, Sealed};
use alloy::primitives::{B256, TxHash};
use futures::Stream;
use pin_project::pin_project;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::SealedBlock;
use reth_transaction_pool::{CanonicalStateUpdate, PoolUpdateKind};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::time::Instant;
use zksync_os_interface::types::AccountDiff;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{
    InteropRootsLogIndex, L1TxSerialId, L2Envelope, ProtocolSemanticVersion, UpgradeTransaction,
    ZkEnvelope, ZkTransaction,
};

pub struct Pool<T> {
    upgrade_transactions: mpsc::Receiver<UpgradeTransaction>,
    interop_roots_subpool: InteropRootsSubpool,
    l1_subpool: L1Subpool,
    l2_subpool: T,
}

impl<T: L2Subpool> Pool<T> {
    pub fn new(
        upgrade_transactions: mpsc::Receiver<UpgradeTransaction>,
        interop_roots_subpool: InteropRootsSubpool,
        l1_subpool: L1Subpool,
        l2_subpool: T,
    ) -> Self {
        Self {
            upgrade_transactions,
            interop_roots_subpool,
            l1_subpool,
            l2_subpool,
        }
    }

    pub async fn best_transactions_stream<'a>(
        &'a mut self,
        next_interop_tx_allowed_after: Instant,
    ) -> TransactionsStream<'a> {
        let interop_stream = self
            .interop_roots_subpool
            .interop_transactions_with_delay(next_interop_tx_allowed_after);
        let mut interop_stream = crate::peekable::Peekable::new(interop_stream);

        let l1_stream = self.l1_subpool.best_transactions_stream();
        let l2_stream = self.l2_subpool.best_transactions_stream();
        let l1_l2_stream = L1L2TxStream {
            last_polled_tx: None,
            l1_stream,
            l2_stream,
        };
        let mut l1_l2_stream = crate::peekable::Peekable::new(l1_l2_stream);

        tokio::select! {
            // If you run this example without `biased;`, the polling order is
            // pseudo-random, and the assertions on the value of count will
            // (probably) fail.
            biased;

            Some(upgrade_tx) = self.upgrade_transactions.recv() => {
                TransactionsStream::upgrade(upgrade_tx)
            }
            Some(_) = interop_stream.peek() => {
                TransactionsStream {
                    upgrade_info: None,
                    stream: interop_stream.boxed_tx_stream(),
                }
            }
            Some(_) = l1_l2_stream.peek() => {
                TransactionsStream {
                    upgrade_info: None,
                    stream: l1_l2_stream.boxed_tx_stream(),
                }
            }

            else => {
                todo!()
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
        let mut system_txs = Vec::new();
        let mut l1_transactions = Vec::new();
        let mut l2_transactions = Vec::new();
        for tx in &replay_record.transactions {
            match tx.envelope() {
                ZkEnvelope::System(system_tx) => {
                    system_txs.push(system_tx.clone());
                }
                ZkEnvelope::L1(l1_tx) => {
                    l1_transactions.push(l1_tx.clone());
                }
                ZkEnvelope::L2(l2_tx) => {
                    l2_transactions.push(*l2_tx.hash());
                }
                ZkEnvelope::Upgrade(_upgrade) => {
                    // todo: upgrade subpool
                    // // consume processed upgrade txs for non-produce commands
                    // if matches!(
                    //     cmd_type,
                    //     BlockCommandType::Rebuild | BlockCommandType::Replay
                    // ) {
                    //     // Skip fetched patch upgrades
                    //     let mut upgrade_tx = self.upgrade_transactions.recv().await.unwrap();
                    //     while upgrade_tx.tx.is_none() {
                    //         upgrade_tx = self.upgrade_transactions.recv().await.unwrap();
                    //     }
                    //
                    //     assert_eq!(upgrade_tx.tx.as_ref(), Some(upgrade));
                    // }
                }
            }
        }

        if !l2_transactions.is_empty() {
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
        }
        let last_interop_log_index = self
            .interop_roots_subpool
            .on_canonical_state_change(system_txs);
        let last_l1_priority_id = self
            .l1_subpool
            .on_canonical_state_change(l1_transactions)
            .await;
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

// todo: move to `types`
#[derive(Debug)]
pub struct UpgradeInfo {
    /// Instruction for the sequencer to NOT execute the upgrade transaction
    /// until the given timestamp.
    /// Represents a timestamp in seconds since UNIX_EPOCH
    pub timestamp: u64,
    /// Which protocol version will be used after the upgrade transaction is executed.
    pub protocol_version: ProtocolSemanticVersion,
    /// Preimages (e.g. force deployments) for the upgrade transaction (if any).
    pub force_preimages: Vec<(B256, Vec<u8>)>,
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

// todo: rename?
pub struct TransactionsStream<'a> {
    pub upgrade_info: Option<UpgradeInfo>,
    pub stream: BoxTxStream<'a>,
}

impl TransactionsStream<'_> {
    fn upgrade(upgrade_tx: UpgradeTransaction) -> Self {
        let upgrade_info = Some(UpgradeInfo {
            timestamp: upgrade_tx.timestamp,
            protocol_version: upgrade_tx.protocol_version,
            force_preimages: upgrade_tx.force_preimages,
        });
        // todo: rename `.tx` to `.envelope`
        if let Some(envelope) = upgrade_tx.tx {
            TransactionsStream {
                upgrade_info,
                stream: todo!(),
                // stream: invalid_box(futures::stream::iter(vec![envelope.into()])),
            }
        } else {
            // fixme: this is different from old impl, is it okay to return empty iterator here?
            TransactionsStream {
                upgrade_info,
                stream: todo!(),
                // stream: invalid_box(futures::stream::empty()),
            }
        }
    }
}
