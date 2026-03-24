use crate::config::RpcConfig;
use crate::result::ToRpcResult;
use crate::rpc_storage::ReadRpcStorage;
use crate::types::QueryLimits;
mod pending;
use pending::{FullTransactionsReceiver, PendingTransactionKind, PendingTransactionsReceiver};
mod registry;
use registry::{FilterKind, FilterRegistry};
mod scan;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{B256, BlockNumber};
use alloy::rpc::types::{
    Filter, FilterBlockOption, FilterChanges, FilterId, Log, PendingTransactionFilterKind,
    Transaction,
};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use scan::scan_logs;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_rpc_api::filter::EthFilterApiServer;
use zksync_os_storage_api::RepositoryError;
use zksync_os_types::L2Envelope;

#[derive(Clone)]
pub struct EthFilterNamespace<RpcStorage, Mempool> {
    storage: RpcStorage,
    query_limits: QueryLimits,
    mempool: Mempool,
    registry: FilterRegistry,
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2Subpool> EthFilterNamespace<RpcStorage, Mempool> {
    pub fn new(config: RpcConfig, storage: RpcStorage, mempool: Mempool) -> Self {
        Self {
            storage,
            query_limits: QueryLimits::new(
                config.max_blocks_per_filter,
                config.max_logs_per_response,
            ),
            mempool,
            registry: FilterRegistry::new(config.stale_filter_ttl),
        }
    }
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2Subpool> EthFilterNamespace<RpcStorage, Mempool> {
    fn install_filter(&self, kind: FilterKind) -> RpcResult<FilterId> {
        let latest_block = self.storage.repository().get_latest_block();
        Ok(self.registry.install(kind, latest_block))
    }

    async fn filter_changes_impl(
        &self,
        id: FilterId,
    ) -> EthFilterResult<FilterChanges<Transaction<L2Envelope>>> {
        let latest_block = self.storage.repository().get_latest_block();

        // start_block is the block from which we should start fetching changes, the next block from
        // the last time changes were polled, in other words the best block at last poll + 1.
        // Returns None when there are no new blocks since the last poll.
        let Some((start_block, kind)) = self.registry.advance(id, latest_block)? else {
            return Ok(FilterChanges::Empty);
        };

        match kind {
            FilterKind::PendingTransaction(filter) => Ok(filter.drain().await),
            FilterKind::Block => {
                let mut block_hashes = Vec::new();
                for block_number in start_block..=latest_block {
                    let Some(block) = self
                        .storage
                        .repository()
                        .get_block_by_number(block_number)?
                    else {
                        return Err(EthFilterError::BlockNotFound(block_number.into()));
                    };
                    block_hashes.push(B256::from(block.header.hash_slow()));
                }
                Ok(FilterChanges::Hashes(block_hashes))
            }
            FilterKind::Log(filter) => {
                let (from_block_number, to_block_number) = match filter.block_option {
                    FilterBlockOption::Range {
                        from_block,
                        to_block,
                    } => self.resolve_range(from_block, to_block)?,
                    FilterBlockOption::AtBlockHash(_) => {
                        // blockHash is equivalent to fromBlock = toBlock = the block number with
                        // hash blockHash
                        // get_logs_in_block_range is inclusive
                        (start_block, latest_block)
                    }
                };
                let logs =
                    self.get_logs_in_block_range(*filter, from_block_number, to_block_number)?;
                Ok(FilterChanges::Logs(logs))
            }
        }
    }

    fn filter_logs_impl(&self, id: FilterId) -> EthFilterResult<Vec<Log>> {
        let filter = self.registry.get_log_filter(&id)?;
        self.logs_impl(filter)
    }

    fn logs_impl(&self, filter: Filter) -> EthFilterResult<Vec<Log>> {
        let (from, to) = match filter.block_option {
            FilterBlockOption::AtBlockHash(block_hash) => {
                let block_id = block_hash.into();
                let Some(block) = self.storage.resolve_block_number(block_id)? else {
                    return Err(EthFilterError::BlockNotFound(block_id));
                };
                (block, block)
            }
            FilterBlockOption::Range {
                from_block,
                to_block,
            } => self.resolve_range(from_block, to_block)?,
        };
        tracing::trace!(from, to, ?filter, "getting filtered logs");
        self.get_logs_in_block_range(filter, from, to)
    }

    fn get_logs_in_block_range(
        &self,
        filter: Filter,
        from: u64,
        to: u64,
    ) -> EthFilterResult<Vec<Log>> {
        if let Some(max_blocks_per_filter) = self
            .query_limits
            .max_blocks_per_filter
            .filter(|limit| to - from > *limit)
        {
            return Err(EthFilterError::QueryExceedsMaxBlocks(max_blocks_per_filter));
        }
        scan_logs(
            self.storage.repository(),
            &filter,
            from,
            to,
            self.query_limits.max_logs_per_response,
        )
    }

    /// Endless future that evicts stale filters every `stale_filter_ttl` interval.
    pub(crate) async fn watch_and_clear_stale_filters(&self) {
        self.registry.watch_and_clear_stale().await
    }

    fn resolve_range(
        &self,
        from_block: Option<BlockNumberOrTag>,
        to_block: Option<BlockNumberOrTag>,
    ) -> EthFilterResult<(BlockNumber, BlockNumber)> {
        let from_block_id = from_block.unwrap_or_default().into();
        let Some(from) = self.storage.resolve_block_number(from_block_id)? else {
            return Err(EthFilterError::BlockNotFound(from_block_id));
        };
        let to_block_id = to_block.unwrap_or_default().into();
        let Some(to) = self.storage.resolve_block_number(to_block_id)? else {
            return Err(EthFilterError::BlockNotFound(to_block_id));
        };
        Ok((from, to))
    }
}

#[async_trait]
impl<RpcStorage: ReadRpcStorage, Mempool: L2Subpool> EthFilterApiServer
    for EthFilterNamespace<RpcStorage, Mempool>
{
    async fn new_filter(&self, filter: Filter) -> RpcResult<FilterId> {
        self.install_filter(FilterKind::Log(Box::new(filter)))
    }

    async fn new_block_filter(&self) -> RpcResult<FilterId> {
        self.install_filter(FilterKind::Block)
    }

    async fn new_pending_transaction_filter(
        &self,
        kind: Option<PendingTransactionFilterKind>,
    ) -> RpcResult<FilterId> {
        let transaction_kind = match kind.unwrap_or_default() {
            PendingTransactionFilterKind::Hashes => {
                let receiver = self.mempool.pending_transactions_listener();
                let pending_txs_receiver = PendingTransactionsReceiver::new(receiver);
                FilterKind::PendingTransaction(PendingTransactionKind::Hashes(pending_txs_receiver))
            }
            PendingTransactionFilterKind::Full => {
                let stream = self.mempool.new_pending_pool_transactions_listener();
                let full_txs_receiver = FullTransactionsReceiver::new(stream);
                FilterKind::PendingTransaction(PendingTransactionKind::FullTransaction(
                    full_txs_receiver,
                ))
            }
        };

        self.install_filter(transaction_kind)
    }

    async fn filter_changes(
        &self,
        id: FilterId,
    ) -> RpcResult<FilterChanges<Transaction<L2Envelope>>> {
        self.filter_changes_impl(id).await.to_rpc_result()
    }

    async fn filter_logs(&self, id: FilterId) -> RpcResult<Vec<Log>> {
        self.filter_logs_impl(id).to_rpc_result()
    }

    async fn uninstall_filter(&self, id: FilterId) -> RpcResult<bool> {
        Ok(self.registry.uninstall(&id))
    }

    async fn logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        Ok(self.logs_impl(filter).to_rpc_result()?)
    }
}

type EthFilterResult<T> = Result<T, EthFilterError>;

/// Errors that can occur in the handler implementation
#[derive(Debug, thiserror::Error)]
pub enum EthFilterError {
    /// Block could not be found by its id (hash/number/tag).
    #[error("block not found")]
    BlockNotFound(BlockId),
    /// Filter not found.
    #[error("filter not found")]
    FilterNotFound(FilterId),
    /// Query scope is too broad.
    #[error("query exceeds max block range {0}")]
    QueryExceedsMaxBlocks(u64),
    /// Query result is too large.
    #[error("query exceeds max results {max_logs}, retry with the range {from_block}-{to_block}")]
    QueryExceedsMaxResults {
        /// Maximum number of logs allowed per response
        max_logs: usize,
        /// Start block of the suggested retry range
        from_block: u64,
        /// End block of the suggested retry range (last successfully processed block)
        to_block: u64,
    },

    #[error(transparent)]
    RepositoryError(#[from] RepositoryError),
}
