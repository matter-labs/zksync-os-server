use crate::L2TransactionPool;
use crate::transaction::L2PooledTransaction;
use alloy::consensus::transaction::Recovered;
use alloy::primitives::TxHash;
use futures::{Stream, StreamExt};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::error::InvalidPoolTransactionError;
use reth_transaction_pool::{BestTransactions, TransactionListenerKind, ValidPoolTransaction};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use zksync_os_types::{L1PriorityEnvelope, L1UpgradeEnvelope, L2Envelope, ZkTransaction};

pub trait TxStream: Stream {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>);
}

pub struct BestTransactionsStream<'a> {
    l1_transactions: &'a mut mpsc::Receiver<L1PriorityEnvelope>,
    upgrade_tx: Option<L1UpgradeEnvelope>,
    pending_transactions_listener: mpsc::Receiver<TxHash>,
    best_l2_transactions:
        Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<L2PooledTransaction>>>>,
    last_polled_l2_tx: Option<Arc<ValidPoolTransaction<L2PooledTransaction>>>,
    peeked_tx: Option<ZkTransaction>,
}

/// Convenience method to stream best L2 transactions
pub fn best_transactions<'a>(
    l2_mempool: &impl L2TransactionPool,
    l1_transactions: &'a mut mpsc::Receiver<L1PriorityEnvelope>,
    upgrade_tx: Option<L1UpgradeEnvelope>,
) -> BestTransactionsStream<'a> {
    let pending_transactions_listener =
        l2_mempool.pending_transactions_listener_for(TransactionListenerKind::All);
    BestTransactionsStream {
        l1_transactions,
        upgrade_tx,
        pending_transactions_listener,
        best_l2_transactions: l2_mempool.best_transactions(),
        last_polled_l2_tx: None,
        peeked_tx: None,
    }
}

impl Stream for BestTransactionsStream<'_> {
    type Item = ZkTransaction;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(tx) = this.peeked_tx.take() {
                return Poll::Ready(Some(tx));
            }

            if let Some(upgrade_tx) = this.upgrade_tx.take() {
                return Poll::Ready(Some(ZkTransaction::from(upgrade_tx)));
            }

            match this.l1_transactions.poll_recv(cx) {
                Poll::Ready(Some(tx)) => return Poll::Ready(Some(tx.into())),
                Poll::Pending => {}
                Poll::Ready(None) => todo!("channel closed"),
            }

            if let Some(tx) = this.best_l2_transactions.next() {
                this.last_polled_l2_tx = Some(tx.clone());
                let (tx, signer) = tx.to_consensus().into_parts();
                let tx = L2Envelope::from(tx);
                return Poll::Ready(Some(Recovered::new_unchecked(tx, signer).into()));
            }

            match this.pending_transactions_listener.poll_recv(cx) {
                // Try to take the next best transaction again
                Poll::Ready(_) => continue,
                // Defer until there is a new pending transaction
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl TxStream for BestTransactionsStream<'_> {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>) {
        let this = self.get_mut();
        let tx = this.last_polled_l2_tx.take().unwrap();
        // Error kind is actually not used internally, but we need to provide it.
        // Reth provides `TxTypeNotSupported` and we do the same just in case.
        this.best_l2_transactions.mark_invalid(
            &tx,
            InvalidPoolTransactionError::Consensus(InvalidTransactionError::TxTypeNotSupported),
        );
    }
}

impl BestTransactionsStream<'_> {
    /// Waits until there is a next transaction and returns a reference to it.
    /// Does not consume the transaction, it will be returned on the next poll.
    /// Returns `None` if the stream is closed.
    pub async fn wait_peek(&mut self) -> Option<&ZkTransaction> {
        if self.peeked_tx.is_none() {
            self.peeked_tx = self.next().await;
        }
        self.peeked_tx.as_ref()
    }
}

pub struct ReplayTxStream {
    iter: Box<dyn Iterator<Item = ZkTransaction> + Send>,
}

impl Stream for ReplayTxStream {
    type Item = ZkTransaction;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.iter.next())
    }
}

impl TxStream for ReplayTxStream {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>) {}
}

impl ReplayTxStream {
    pub fn new(txs: Vec<ZkTransaction>) -> Self {
        Self {
            iter: Box::new(txs.into_iter()),
        }
    }
}
