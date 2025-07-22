use alloy::consensus::EthereumTxEnvelope;
use alloy::consensus::transaction::Recovered;
use alloy::primitives::TxHash;
use futures::Stream;
use reth_transaction_pool::{
    BestTransactions, EthPooledTransaction, PoolResult, PoolTransaction, TransactionListenerKind,
    TransactionOrigin, TransactionPoolExt, ValidPoolTransaction,
};
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use zksync_os_types::{L2Envelope, L2Transaction};

#[allow(async_fn_in_trait)]
#[auto_impl::auto_impl(&, Box, Arc)]
pub trait L2TransactionPool:
    TransactionPoolExt<Transaction = EthPooledTransaction> + Send + Sync + Debug + 'static
{
    /// Convenience method to add a local L2 transaction
    async fn add_l2_transaction(&self, transaction: L2Transaction) -> PoolResult<TxHash> {
        let (envelope, signer) = transaction.into_parts();
        let envelope = EthereumTxEnvelope::try_from(envelope)
            .expect("tried to insert an EIP-4844 transaction without sidecar into mempool");
        let transaction = Recovered::new_unchecked(envelope, signer);
        self.add_transaction(
            TransactionOrigin::Local,
            EthPooledTransaction::from_pooled(transaction),
        )
        .await
    }

    /// Convenience method to stream best L2 transactions
    fn best_l2_transactions(&self) -> impl Stream<Item = L2Transaction> + Send + 'static {
        let pending_transactions_listener =
            self.pending_transactions_listener_for(TransactionListenerKind::All);
        BestL2Transactions {
            pending_transactions_listener,
            best_transactions: self.best_transactions(),
        }
    }
}

struct BestL2Transactions {
    pending_transactions_listener: mpsc::Receiver<TxHash>,
    best_transactions:
        Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<EthPooledTransaction>>>>,
}

impl Stream for BestL2Transactions {
    type Item = L2Transaction;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(tx) = this.best_transactions.next() {
                let (tx, signer) = tx.to_consensus().into_parts();
                let tx = L2Envelope::from(tx);
                return Poll::Ready(Some(Recovered::new_unchecked(tx, signer)));
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
