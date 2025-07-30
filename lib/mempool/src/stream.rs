use crate::RethPool;
use alloy::consensus::transaction::Recovered;
use alloy::primitives::TxHash;
use futures::Stream;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::error::InvalidPoolTransactionError;
use reth_transaction_pool::{
    BestTransactions, EthPooledTransaction, TransactionListenerKind, TransactionPool,
    ValidPoolTransaction,
};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::{Mutex, mpsc};
use zksync_os_types::{L1Envelope, L2Envelope, ZkTransaction};

pub trait TxStream: Stream {
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>);
}

pub struct BestTransactionsStream {
    l1_transactions: Arc<Mutex<mpsc::Receiver<L1Envelope>>>,
    pending_transactions_listener: mpsc::Receiver<TxHash>,
    best_l2_transactions:
        Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<EthPooledTransaction>>>>,
    last_polled_l2_tx: Option<Arc<ValidPoolTransaction<EthPooledTransaction>>>,
}

/// Convenience method to stream best L2 transactions
pub fn best_transactions<
    Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory,
>(
    l2_mempool: &RethPool<Client>,
    l1_transactions: Arc<Mutex<mpsc::Receiver<L1Envelope>>>,
) -> impl TxStream<Item = ZkTransaction> + Send + 'static {
    let pending_transactions_listener =
        l2_mempool.pending_transactions_listener_for(TransactionListenerKind::All);
    BestTransactionsStream {
        l1_transactions,
        pending_transactions_listener,
        best_l2_transactions: l2_mempool.best_transactions(),
        last_polled_l2_tx: None,
    }
}

impl Stream for BestTransactionsStream {
    type Item = ZkTransaction;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this
                .l1_transactions
                .try_lock()
                .expect("more than one reader on l1 transaction channel")
                .poll_recv(cx)
            {
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

impl TxStream for BestTransactionsStream {
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
    fn mark_last_tx_as_invalid(self: Pin<&mut Self>) {
        panic!("Unexpected `mark_invalid` call on `ReplayTxStream`");
    }
}

impl ReplayTxStream {
    pub fn new(txs: Vec<ZkTransaction>) -> Self {
        Self {
            iter: Box::new(txs.into_iter()),
        }
    }
}
