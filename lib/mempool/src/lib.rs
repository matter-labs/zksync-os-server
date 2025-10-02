mod reth;
pub use reth::RethPool;

mod stream;
pub use stream::{BestTransactionsStream, ReplayTxStream, TxStream, best_transactions};

mod traits;
pub use traits::L2TransactionPool;

mod transaction;
pub use transaction::L2PooledTransaction;

// Re-export some of the reth mempool's types.
pub use reth_transaction_pool::error::PoolError;
pub use reth_transaction_pool::{
    CanonicalStateUpdate, NewSubpoolTransactionStream, NewTransactionEvent, PoolUpdateKind,
    TransactionPool as RethTransactionPool, TransactionPoolExt as RethTransactionPoolExt,
};

use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::test_utils::OkValidator;
use reth_transaction_pool::validate::EthTransactionValidatorBuilder;
use reth_transaction_pool::{CoinbaseTipOrdering, PoolConfig};

pub fn in_memory<Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory>(
    client: Client,
    max_input_bytes: usize,
) -> RethPool {
    let blob_store = NoopBlobStore::default();
    RethPool::new(
        OkValidator::default(),
        CoinbaseTipOrdering::default(),
        blob_store,
        PoolConfig::default(),
    )
}
