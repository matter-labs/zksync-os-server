mod stream;
pub use stream::{BestTransactionsStream, ReplayTxStream, TxStream, best_transactions};

mod traits;
pub use traits::L2TransactionPool;

mod transaction;
pub use transaction::L2PooledTransaction;

mod config;
pub use config::TxValidatorConfig;

mod interop_tx_stream;
pub use interop_tx_stream::{InteropRootTransactions, InteropRootsTxPool};

mod metrics;

// Re-export some of the reth mempool's types.
pub use reth_transaction_pool::error::PoolError;
pub use reth_transaction_pool::{
    CanonicalStateUpdate, NewSubpoolTransactionStream, NewTransactionEvent, PoolConfig,
    PoolUpdateKind, SubPoolLimit,
};

use crate::metrics::ViseRecorder;
use crate::traits::RethPool;
use reth_transaction_pool::CoinbaseTipOrdering;
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::validate::EthTransactionValidatorBuilder;
use zksync_os_reth_compat::provider::ZkProviderFactory;
use zksync_os_storage_api::{ReadRepository, ReadStateHistory};

pub fn in_memory(
    zk_provider_factory: ZkProviderFactory<
        impl ReadStateHistory + Clone,
        impl ReadRepository + Clone,
    >,
    pool_config: PoolConfig,
    validator_config: TxValidatorConfig,
) -> impl L2TransactionPool {
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
