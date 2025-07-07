use crate::traits::L2TransactionPool;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::{
    CoinbaseTipOrdering, EthPooledTransaction, EthTransactionValidator, Pool,
};

pub type RethPool<Client> = Pool<
    EthTransactionValidator<Client, EthPooledTransaction>,
    CoinbaseTipOrdering<EthPooledTransaction>,
    NoopBlobStore,
>;

impl<Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory + 'static>
    L2TransactionPool for RethPool<Client>
{
}
