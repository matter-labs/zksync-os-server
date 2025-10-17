use crate::reth_state::ZkClient;
use crate::transaction::L2PooledTransaction;
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::{
    AddedTransactionOutcome, CoinbaseTipOrdering, EthTransactionValidator, Pool, PoolResult,
    PoolTransaction, TransactionOrigin, TransactionPoolExt,
};
use std::fmt::Debug;
use zksync_os_storage_api::{ReadRepository, ReadStateHistory};
use zksync_os_types::L2Transaction;

pub(crate) type RethPool<State, Repository> = Pool<
    EthTransactionValidator<ZkClient<State, Repository>, L2PooledTransaction>,
    CoinbaseTipOrdering<L2PooledTransaction>,
    NoopBlobStore,
>;

#[allow(async_fn_in_trait)]
#[auto_impl::auto_impl(&, Box, Arc)]
pub trait L2TransactionPool:
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
}

impl<State: ReadStateHistory + Clone, Repository: ReadRepository + Clone> L2TransactionPool
    for RethPool<State, Repository>
{
}
