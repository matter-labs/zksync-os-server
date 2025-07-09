mod l1_pool;
mod reth;
mod traits;

pub use crate::reth::RethPool;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_storage_api::StateProviderFactory;
pub use reth_transaction_pool::error::PoolError;
pub use reth_transaction_pool::{
    CanonicalStateUpdate, PoolUpdateKind, TransactionPool as RethTransactionPool,
    TransactionPoolExt as RethTransactionPoolExt,
};
pub use traits::L2TransactionPool;

use crate::l1_pool::{L1Mempool, L1Pool};
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::validate::EthTransactionValidatorBuilder;
use reth_transaction_pool::{CoinbaseTipOrdering, PoolConfig};
use zksync_os_types::L1Transaction;

pub type DynL1Pool = Box<dyn L1Pool>;

pub fn in_memory<Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory>(
    client: Client,
    forced_tx: L1Transaction,
    max_input_bytes: usize,
) -> (DynL1Pool, RethPool<Client>) {
    let blob_store = NoopBlobStore::default();
    (
        Box::new(L1Mempool::new(forced_tx)),
        RethPool::new(
            EthTransactionValidatorBuilder::new(client)
                .with_max_tx_input_bytes(max_input_bytes)
                .build(blob_store),
            CoinbaseTipOrdering::default(),
            blob_store,
            PoolConfig::default(),
        ),
    )
}
