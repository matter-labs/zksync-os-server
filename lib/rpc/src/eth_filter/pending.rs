use alloy::consensus::transaction::{Recovered, TransactionInfo};
use alloy::primitives::TxHash;
use alloy::rpc::types::{FilterChanges, Transaction};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use zksync_os_mempool::{L2PooledTransaction, NewSubpoolTransactionStream};
use zksync_os_types::L2Envelope;

/// Represents the kind of pending transaction data that can be retrieved.
///
/// This enum differentiates between two kinds of pending transaction data:
/// - Just the transaction hashes.
/// - Full transaction details.
#[derive(Debug, Clone)]
pub(crate) enum PendingTransactionKind {
    Hashes(PendingTransactionsReceiver),
    FullTransaction(FullTransactionsReceiver),
}

impl PendingTransactionKind {
    pub(crate) async fn drain(&self) -> FilterChanges<Transaction<L2Envelope>> {
        match self {
            Self::Hashes(receiver) => receiver.drain().await,
            Self::FullTransaction(receiver) => receiver.drain().await,
        }
    }
}

/// A receiver for pending transactions that returns all new transactions since the last poll.
#[derive(Debug, Clone)]
pub(crate) struct PendingTransactionsReceiver {
    receiver: Arc<Mutex<mpsc::Receiver<TxHash>>>,
}

impl PendingTransactionsReceiver {
    pub(crate) fn new(receiver: mpsc::Receiver<TxHash>) -> Self {
        Self {
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }

    /// Returns all new pending transactions received since the last poll.
    async fn drain(&self) -> FilterChanges<Transaction<L2Envelope>> {
        let mut pending_txs = Vec::new();
        let mut prepared_stream = self.receiver.lock().await;

        while let Ok(tx_hash) = prepared_stream.try_recv() {
            pending_txs.push(tx_hash);
        }

        FilterChanges::Hashes(pending_txs)
    }
}

/// A structure to manage and provide access to a stream of full transaction details.
#[derive(Debug, Clone)]
pub(crate) struct FullTransactionsReceiver {
    txs_stream: Arc<Mutex<NewSubpoolTransactionStream<L2PooledTransaction>>>,
}

impl FullTransactionsReceiver {
    pub(crate) fn new(txs_stream: NewSubpoolTransactionStream<L2PooledTransaction>) -> Self {
        Self {
            txs_stream: Arc::new(Mutex::new(txs_stream)),
        }
    }

    /// Returns all new pending transactions received since the last poll.
    async fn drain(&self) -> FilterChanges<Transaction<L2Envelope>> {
        let mut pending_txs = Vec::new();
        let mut prepared_stream = self.txs_stream.lock().await;

        while let Ok(tx) = prepared_stream.try_recv() {
            let (tx, signer) = tx.transaction.to_consensus().into_parts();
            let tx = L2Envelope::from(tx);
            pending_txs.push(Transaction::from_transaction(
                Recovered::new_unchecked(tx, signer),
                TransactionInfo::default(),
            ));
        }
        FilterChanges::Transactions(pending_txs)
    }
}
