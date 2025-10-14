mod reth;
pub use reth::RethPool;

mod stream;
pub use stream::{BestTransactionsStream, ReplayTxStream, TxStream, best_transactions};

mod traits;
pub use traits::L2TransactionPool;

mod transaction;
pub use transaction::L2PooledTransaction;

mod config;
pub use config::TxValidatorConfig;

mod metrics;

// Re-export some of the reth mempool's types.
pub use reth_transaction_pool::error::PoolError;
pub use reth_transaction_pool::{
    CanonicalStateUpdate, NewSubpoolTransactionStream, NewTransactionEvent, PoolConfig,
    PoolUpdateKind, SubPoolLimit, TransactionPool as RethTransactionPool,
    TransactionPoolExt as RethTransactionPoolExt,
};

use crate::metrics::ViseRecorder;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::CoinbaseTipOrdering;
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::validate::EthTransactionValidatorBuilder;

pub fn in_memory<Client: ChainSpecProvider<ChainSpec: EthereumHardforks> + StateProviderFactory>(
    client: Client,
    pool_config: PoolConfig,
    validator_config: TxValidatorConfig,
) -> RethPool<Client> {
    let blob_store = NoopBlobStore::default();
    // Use `ViseRecorder` during mempool initialization to register metrics. This will make sure
    // reth mempool metrics are propagated to `vise` collector. Only code inside the closure is
    // affected.
    ::metrics::with_local_recorder(&ViseRecorder, || {
        RethPool::new(
            EthTransactionValidatorBuilder::new(client)
                .no_prague()
                .with_max_tx_input_bytes(validator_config.max_input_bytes)
                .build(blob_store),
            CoinbaseTipOrdering::default(),
            blob_store,
            pool_config,
        )
    })
}
