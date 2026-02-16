use crate::metrics::ViseRecorder;
use crate::{L2PooledTransaction, TxValidatorConfig};
use alloy::consensus::transaction::Recovered;
use alloy::primitives::TxHash;
use futures::Stream;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::error::InvalidPoolTransactionError;
use reth_transaction_pool::validate::EthTransactionValidatorBuilder;
use reth_transaction_pool::{
    AddedTransactionOutcome, CoinbaseTipOrdering, EthTransactionValidator, Pool, PoolConfig,
    PoolResult, PoolTransaction, TransactionListenerKind, TransactionOrigin, TransactionPoolExt,
};
use reth_transaction_pool::{BestTransactions, ValidPoolTransaction};
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use zksync_os_reth_compat::provider::ZkProviderFactory;
use zksync_os_storage_api::{ReadRepository, ReadStateHistory};
use zksync_os_types::{L2Transaction, ZkTransaction};

pub(crate) type RethPool<State, Repository> = Pool<
    EthTransactionValidator<ZkProviderFactory<State, Repository>, L2PooledTransaction>,
    CoinbaseTipOrdering<L2PooledTransaction>,
    NoopBlobStore,
>;

#[allow(async_fn_in_trait)]
#[auto_impl::auto_impl(&, Box, Arc)]
pub trait L2Subpool:
    TransactionPoolExt<Transaction = L2PooledTransaction> + Send + Sync + Debug + 'static
{
    /// Convenience method to add a local L2 transaction
    fn add_l2_transaction(
        &self,
        transaction: L2Transaction,
    ) -> impl Future<Output = PoolResult<AddedTransactionOutcome>> + Send {
        self.add_transaction(
            TransactionOrigin::Local,
            L2PooledTransaction::from_pooled(transaction),
        )
    }

    fn best_transactions_stream(&self) -> L2TransactionsStream {
        L2TransactionsStream {
            inner: Arc::new(Mutex::new(Inner {
                last_polled_tx: None,
                txs: self.best_transactions(),
            })),
            pending_txs_listener: self
                .pending_transactions_listener_for(TransactionListenerKind::All),
        }
    }
}

impl<State: ReadStateHistory + Clone, Repository: ReadRepository + Clone> L2Subpool
    for RethPool<State, Repository>
{
}

pub struct L2TransactionsStream {
    inner: Arc<Mutex<Inner>>,
    pending_txs_listener: mpsc::Receiver<TxHash>,
}

pub(crate) struct L2TransactionsStreamMarker {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    last_polled_tx: Option<Arc<ValidPoolTransaction<L2PooledTransaction>>>,
    txs: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<L2PooledTransaction>>>>,
}

impl Stream for L2TransactionsStream {
    type Item = ZkTransaction;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            {
                let mut inner = this.inner.lock().unwrap();
                if let Some(tx) = inner.txs.next() {
                    inner.last_polled_tx = Some(tx.clone());
                    let (tx, signer) = tx.to_consensus().into_parts();
                    return Poll::Ready(Some(Recovered::new_unchecked(tx, signer).into()));
                }
            }

            match this.pending_txs_listener.poll_recv(cx) {
                // Try to take the next best transaction again
                Poll::Ready(_) => continue,
                // Defer until there is a new pending transaction
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl L2TransactionsStream {
    pub(crate) fn marker(&self) -> L2TransactionsStreamMarker {
        let inner = self.inner.clone();
        L2TransactionsStreamMarker { inner }
    }
}

impl L2TransactionsStreamMarker {
    pub(crate) fn mark_last_tx_as_invalid(&self) {
        let mut inner = self.inner.lock().unwrap();
        let Some(tx) = inner.last_polled_tx.take() else {
            tracing::error!("tried to mark non-existing L2 transaction as invalid");
            return;
        };
        // Error kind is actually not used internally, but we need to provide it.
        // Reth provides `TxTypeNotSupported` and we do the same just in case.
        inner.txs.mark_invalid(
            &tx,
            InvalidPoolTransactionError::Consensus(InvalidTransactionError::TxTypeNotSupported),
        );
    }
}

pub fn in_memory(
    zk_provider_factory: ZkProviderFactory<
        impl ReadStateHistory + Clone,
        impl ReadRepository + Clone,
    >,
    pool_config: PoolConfig,
    validator_config: TxValidatorConfig,
) -> impl L2Subpool {
    let blob_store = NoopBlobStore::default();
    // Use `ViseRecorder` during mempool initialization to register metrics. This will make sure
    // reth mempool metrics are propagated to `vise` collector. Only code inside the closure is
    // affected.
    ::metrics::with_local_recorder(&ViseRecorder, move || {
        RethPool::new(
            EthTransactionValidatorBuilder::new(zk_provider_factory)
                .no_prague()
                .with_max_tx_input_bytes(validator_config.max_input_bytes)
                // set tx_fee_cap to 0, effectively disabling the tx fee checks in the reth mempool
                // this is necessary to process transactions with more than 1e18 tx fee
                .set_tx_fee_cap(0)
                .build(blob_store),
            CoinbaseTipOrdering::default(),
            blob_store,
            pool_config,
        )
    })
}
