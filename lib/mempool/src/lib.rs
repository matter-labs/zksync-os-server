mod stream;
pub use stream::{BestTransactionsStream, ReplayTxStream, TxStream, best_transactions};

mod traits;
pub use traits::L2TransactionPool;

mod transaction;
pub use transaction::L2PooledTransaction;

mod config;
pub use config::TxValidatorConfig;

mod metrics;
mod reth_state;

// Re-export some of the reth mempool's types.
pub use reth_transaction_pool::error::PoolError;
pub use reth_transaction_pool::{
    CanonicalStateUpdate, NewSubpoolTransactionStream, NewTransactionEvent, PoolConfig,
    PoolUpdateKind, SubPoolLimit,
};

use crate::metrics::ViseRecorder;
use crate::reth_state::ZkClient;
use crate::traits::RethPool;
use reth_transaction_pool::CoinbaseTipOrdering;
use reth_transaction_pool::blobstore::NoopBlobStore;
use reth_transaction_pool::validate::EthTransactionValidatorBuilder;
use zksync_os_storage_api::{ReadRepository, ReadStateHistory};

pub fn in_memory<State: ReadStateHistory + Clone, Repository: ReadRepository + Clone>(
    state: State,
    repository: Repository,
    chain_id: u64,
    pool_config: PoolConfig,
    validator_config: TxValidatorConfig,
) -> impl L2TransactionPool {
    let client = ZkClient::new(state, repository, chain_id);
    let blob_store = NoopBlobStore::default();
    // Use `ViseRecorder` during mempool initialization to register metrics. This will make sure
    // reth mempool metrics are propagated to `vise` collector. Only code inside the closure is
    // affected.
    ::metrics::with_local_recorder(&ViseRecorder, move || {
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
