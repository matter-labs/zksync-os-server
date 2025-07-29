mod reth;
mod stream;
mod traits;

pub use crate::reth::RethPool;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_storage_api::StateProviderFactory;
pub use reth_transaction_pool::error::PoolError;
pub use reth_transaction_pool::{
    CanonicalStateUpdate, PoolUpdateKind, TransactionPool as RethTransactionPool,
    TransactionPoolExt as RethTransactionPoolExt,
};
pub use stream::{BestTransactionsStream, ReplayTxStream, TxStream, best_transactions};
pub use traits::L2TransactionPool;

use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::validate::EthTransactionValidatorBuilder;
use reth_transaction_pool::{CoinbaseTipOrdering, PoolConfig};

pub fn in_memory<Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory>(
    client: Client,
    max_input_bytes: usize,
) -> RethPool<Client> {
    let blob_store = NoopBlobStore::default();
    RethPool::new(
        EthTransactionValidatorBuilder::new(client)
            .with_max_tx_input_bytes(max_input_bytes)
            .build(blob_store),
        CoinbaseTipOrdering::default(),
        blob_store,
        PoolConfig::default(),
    )
}
