use crate::eth_impl::build_api_tx;
use alloy::primitives::Address;
use alloy::rpc::types::txpool::{TxpoolContent, TxpoolInspect, TxpoolInspectSummary, TxpoolStatus};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use std::collections::BTreeMap;
use std::sync::Arc;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_mempool::{L2PooledTransaction, ValidPoolTransaction};
use zksync_os_rpc_api::txpool::TxpoolApiServer;
use zksync_os_rpc_api::types::ZkApiTransaction;
use zksync_os_types::ZkTransaction;

pub struct TxpoolNamespace<Mempool> {
    mempool: Mempool,
}

impl<Mempool: L2Subpool> TxpoolNamespace<Mempool> {
    pub fn new(mempool: Mempool) -> Self {
        Self { mempool }
    }
}

type PoolTx = Arc<ValidPoolTransaction<L2PooledTransaction>>;

fn inspect_summary(pool_tx: &PoolTx) -> TxpoolInspectSummary {
    pool_tx.transaction.transaction.clone().into_inner().into()
}

fn content_tx(pool_tx: &PoolTx) -> ZkApiTransaction {
    let zk_tx: ZkTransaction = pool_tx.transaction.transaction.clone().into();
    build_api_tx(zk_tx, None)
}

fn insert_by_sender<T>(
    map: &mut BTreeMap<Address, BTreeMap<String, T>>,
    pool_tx: &PoolTx,
    value: T,
) {
    map.entry(pool_tx.sender())
        .or_default()
        .insert(pool_tx.nonce().to_string(), value);
}

#[async_trait]
impl<Mempool: L2Subpool> TxpoolApiServer for TxpoolNamespace<Mempool> {
    async fn inspect(&self) -> RpcResult<TxpoolInspect> {
        let mut result = TxpoolInspect::default();
        let all = self.mempool.all_transactions();
        for pool_tx in &all.pending {
            insert_by_sender(&mut result.pending, pool_tx, inspect_summary(pool_tx));
        }
        for pool_tx in &all.queued {
            insert_by_sender(&mut result.queued, pool_tx, inspect_summary(pool_tx));
        }
        Ok(result)
    }

    async fn content(&self) -> RpcResult<TxpoolContent<ZkApiTransaction>> {
        let mut result: TxpoolContent<ZkApiTransaction> = TxpoolContent::default();
        let all = self.mempool.all_transactions();
        for pool_tx in &all.pending {
            insert_by_sender(&mut result.pending, pool_tx, content_tx(pool_tx));
        }
        for pool_tx in &all.queued {
            insert_by_sender(&mut result.queued, pool_tx, content_tx(pool_tx));
        }
        Ok(result)
    }

    async fn status(&self) -> RpcResult<TxpoolStatus> {
        let (pending, queued) = self.mempool.pending_and_queued_txn_count();
        Ok(TxpoolStatus {
            pending: pending as u64,
            queued: queued as u64,
        })
    }
}
