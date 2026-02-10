use crate::transaction::L2PooledTransaction;
use crate::{InteropRootTransactions, L2TransactionPool};
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
use zksync_os_types::{
    L1PriorityEnvelope, L2Envelope, SystemTxEnvelope, UpgradeTransaction, ZkTransaction,
};

pub trait TxStream: Stream {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>);
}

pub struct BestTransactionsStream<'a> {
    l1_transactions: &'a mut mpsc::Receiver<L1PriorityEnvelope>,
    pending_upgrade_transactions: &'a mut mpsc::Receiver<UpgradeTransaction>,
    sl_chain_id_update_transactions: &'a mut mpsc::Receiver<SystemTxEnvelope>,
    interop_transactions: InteropRootTransactions,
    pending_transactions_listener: mpsc::Receiver<TxHash>,
    best_l2_transactions:
        Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<L2PooledTransaction>>>>,
    last_polled_l2_tx: Option<Arc<ValidPoolTransaction<L2PooledTransaction>>>,
    peeked_tx: Option<ZkTransaction>,
    peeked_upgrade_info: Option<UpgradeTransaction>,
    txs_already_provided: bool,
    provide_only_interop_txs: bool,
}

/// Convenience method to stream best L2 transactions
pub fn best_transactions<'a>(
    l2_mempool: &impl L2TransactionPool,
    l1_transactions: &'a mut mpsc::Receiver<L1PriorityEnvelope>,
    sl_chain_id_update_transactions: &'a mut mpsc::Receiver<SystemTxEnvelope>,
    interop_transactions: InteropRootTransactions,
    pending_upgrade_transactions: &'a mut mpsc::Receiver<UpgradeTransaction>,
) -> BestTransactionsStream<'a> {
    let pending_transactions_listener =
        l2_mempool.pending_transactions_listener_for(TransactionListenerKind::All);
    BestTransactionsStream {
        l1_transactions,
        sl_chain_id_update_transactions,
        interop_transactions,
        pending_upgrade_transactions,
        pending_transactions_listener,
        best_l2_transactions: l2_mempool.best_transactions(),
        last_polled_l2_tx: None,
        peeked_tx: None,
        peeked_upgrade_info: None,
        txs_already_provided: false,
        provide_only_interop_txs: false,
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

            // We only should provide an upgrade transaction if it's the first one in the stream for this block.
            if !this.txs_already_provided {
                match this.pending_upgrade_transactions.poll_recv(cx) {
                    Poll::Ready(Some(tx)) => {
                        this.peeked_upgrade_info = Some(tx.clone());
                        if let Some(envelope) = tx.tx {
                            return Poll::Ready(Some(envelope.into()));
                        }
                        // If there is no upgrade transaction (patch-only upgrade), continue to the next step.
                        // We already set the upgrade info, so protocol version will be updated once
                        // the first transaction will arrive.
                    }
                    Poll::Pending => {}
                    Poll::Ready(None) => return Poll::Ready(None),
                }
            }

            // We only should provide an SL chain id update transaction if it's the first one in the stream for this block.
            if !this.txs_already_provided {
                match this.sl_chain_id_update_transactions.poll_recv(cx) {
                    Poll::Ready(Some(tx)) => {
                        return Poll::Ready(Some(tx.into()));
                    }
                    Poll::Pending => {}
                    Poll::Ready(None) => return Poll::Ready(None),
                }
            }

            if !this.txs_already_provided || this.provide_only_interop_txs {
                match this.interop_transactions.poll_next_unpin(cx) {
                    Poll::Ready(Some(tx)) => {
                        // If first transaction in stream was interop one we should provide only interop transactions
                        this.provide_only_interop_txs = true;
                        return Poll::Ready(Some(tx.into()));
                    }
                    Poll::Pending if this.provide_only_interop_txs => {
                        return Poll::Pending;
                    }
                    // This arm is reachable in case if first transaction wasn't interop and we can execute other transactions
                    Poll::Pending => {}
                    Poll::Ready(None) => return Poll::Ready(None),
                }
            }

            match this.l1_transactions.poll_recv(cx) {
                Poll::Ready(Some(tx)) => return Poll::Ready(Some(tx.into())),
                Poll::Pending => {}
                Poll::Ready(None) => return Poll::Ready(None),
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
    /// Returns `Some(None)` if there is a transaction in the stream, but it's not an upgrade transaction.
    /// Returns `Some(Some(upgrade_tx))` if the next transaction is an upgrade transaction.
    /// and if the peeked transaction is an interop transaction.
    // TODO: this interface leaks implementation details about the internal structure, and in general
    // this information is only needed for the `BlockContextProvider` which already has access to the stream.
    // This was introduced only because upgrade transaction can appear after we started waiting for the
    // first tx, and we need protocol upgrade info to initialize block context.
    // Consider refactoring this later.
    pub async fn wait_peek(&mut self) -> Option<Option<UpgradeTransaction>> {
        if self.peeked_tx.is_none() {
            self.peeked_tx = self.next().await;
            self.txs_already_provided = true; // TODO: implicit expectation that this method is _guaranteed_ to be called before using the stream.
        }

        // Return `None` if the stream is closed.
        #[allow(clippy::question_mark)]
        if self.peeked_tx.is_none() {
            return None;
        }

        Some(self.peeked_upgrade_info.clone())
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
