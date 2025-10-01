use crate::config::RpcConfig;
use crate::eth_impl::build_api_log;
use crate::metrics::API_METRICS;
use crate::result::ToRpcResult;
use crate::rpc_storage::ReadRpcStorage;
use crate::types::QueryLimits;
use alloy::consensus::transaction::{Recovered, TransactionInfo};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{B256, BlockNumber, TxHash, U128};
use alloy::rpc::types::{
    Filter, FilterBlockOption, FilterChanges, FilterId, Log, PendingTransactionFilterKind,
    Transaction,
};
use async_trait::async_trait;
use dashmap::DashMap;
use jsonrpsee::core::RpcResult;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};
use tokio::time::MissedTickBehavior;
use zksync_os_mempool::{L2PooledTransaction, L2TransactionPool, NewSubpoolTransactionStream};
use zksync_os_rpc_api::filter::EthFilterApiServer;
use zksync_os_storage_api::RepositoryError;
use zksync_os_types::L2Envelope;

#[derive(Clone)]
pub struct EthFilterNamespace<RpcStorage, Mempool> {
    storage: RpcStorage,
    query_limits: QueryLimits,
    /// Duration since the last filter poll, after which the filter is considered stale
    stale_filter_ttl: Duration,
    mempool: Mempool,
    active_filters: Arc<DashMap<FilterId, ActiveFilter>>,
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool>
    EthFilterNamespace<RpcStorage, Mempool>
{
    pub fn new(config: RpcConfig, storage: RpcStorage, mempool: Mempool) -> Self {
        let query_limits =
            QueryLimits::new(config.max_blocks_per_filter, config.max_logs_per_response);
        let this = Self {
            storage,
            query_limits,
            stale_filter_ttl: config.stale_filter_ttl,
            mempool,
            active_filters: Arc::new(DashMap::new()),
        };

        // todo: dangling task, respect stop_receiver and register task when there is a proper system
        let this_clone = this.clone();
        tokio::spawn(async move {
            this_clone.watch_and_clear_stale_filters().await;
        });

        this
    }
}

impl<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool>
    EthFilterNamespace<RpcStorage, Mempool>
{
    fn install_filter(&self, kind: FilterKind) -> RpcResult<FilterId> {
        let last_poll_block_number = self.storage.repository().get_latest_block();
        let id = FilterId::Str(format!("0x{:x}", U128::random()));

        self.active_filters.insert(
            id.clone(),
            ActiveFilter {
                block: last_poll_block_number,
                last_poll_timestamp: Instant::now(),
                kind,
            },
        );
        Ok(id)
    }

    async fn filter_changes_impl(
        &self,
        id: FilterId,
    ) -> EthFilterResult<FilterChanges<Transaction<L2Envelope>>> {
        let latest_block = self.storage.repository().get_latest_block();

        // start_block is the block from which we should start fetching changes, the next block from
        // the last time changes were polled, in other words the best block at last poll + 1
        let (start_block, kind) = {
            let mut filter = self
                .active_filters
                .get_mut(&id)
                .ok_or(EthFilterError::FilterNotFound(id))?;

            if filter.block > latest_block {
                // no new blocks since the last poll
                return Ok(FilterChanges::Empty);
            }

            // update filter
            // we fetch all changes from [filter.block..best_block], so we advance the filter's
            // block to `best_block +1`, the next from which we should start fetching changes again
            let mut block = latest_block + 1;
            std::mem::swap(&mut filter.block, &mut block);
            filter.last_poll_timestamp = Instant::now();

            (block, filter.kind.clone())
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
        let filter = {
            if let FilterKind::Log(ref filter) = self
                .active_filters
                .get(&id)
                .ok_or_else(|| EthFilterError::FilterNotFound(id.clone()))?
                .kind
            {
                filter.clone()
            } else {
                // Not a log filter
                return Err(EthFilterError::FilterNotFound(id));
            }
        };

        self.logs_impl(*filter)
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

        let is_multi_block_range = from != to;
        let total_scanned_blocks = to - from + 1;
        let mut tp_scanned_blocks = 0u64;
        let mut fp_scanned_blocks = 0u64;
        let mut negative_scanned_blocks = 0u64;
        let mut logs = Vec::new();
        for number in from..=to {
            if let Some(block) = self.storage.repository().get_block_by_number(number)? {
                let mut log_index_in_block = 0u64;
                if filter.matches_bloom(block.header.logs_bloom) {
                    tracing::trace!(
                        number,
                        ?filter,
                        "Block matches bloom filter, scanning receipts",
                    );
                    let stored_txs = block
                        .unseal()
                        .body
                        .transactions
                        .into_iter()
                        .map(|hash| {
                            self.storage
                                .repository()
                                .get_stored_transaction(hash)?
                                .ok_or(EthFilterError::BlockNotFound(number.into()))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut at_least_one_log_added = false;
                    for tx in stored_txs {
                        for inner_log in tx.receipt.logs() {
                            if filter.matches(inner_log) {
                                logs.push(build_api_log(
                                    *tx.tx.hash(),
                                    inner_log.clone(),
                                    tx.meta.clone(),
                                    log_index_in_block - tx.meta.number_of_logs_before_this_tx,
                                ));
                                at_least_one_log_added = true;
                            }
                            log_index_in_block += 1;
                        }
                    }
                    if at_least_one_log_added {
                        tp_scanned_blocks += 1;
                    } else {
                        fp_scanned_blocks += 1;
                    }

                    // size check but only if range is multiple blocks, so we always return all
                    // logs of a single block
                    if let Some(max_logs_per_response) = self.query_limits.max_logs_per_response
                        && is_multi_block_range
                        && logs.len() > max_logs_per_response
                    {
                        let suggested_to = number.saturating_sub(1);
                        return Err(EthFilterError::QueryExceedsMaxResults {
                            max_logs: max_logs_per_response,
                            from_block: from,
                            to_block: suggested_to,
                        });
                    }
                } else {
                    negative_scanned_blocks += 1;
                }
            }
        }

        API_METRICS.get_logs_scanned_blocks[&"total"].observe(total_scanned_blocks);
        API_METRICS.get_logs_scanned_blocks[&"true_positive"].observe(tp_scanned_blocks);
        API_METRICS.get_logs_scanned_blocks[&"false_positive"].observe(fp_scanned_blocks);
        API_METRICS.get_logs_scanned_blocks[&"negative"].observe(negative_scanned_blocks);

        Ok(logs)
    }

    /// Endless future that [`Self::clear_stale_filters`] every `stale_filter_ttl` interval.
    /// Nonetheless, this endless future frees the thread at every await point.
    async fn watch_and_clear_stale_filters(&self) {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + self.stale_filter_ttl,
            self.stale_filter_ttl,
        );
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            self.clear_stale_filters(Instant::now()).await;
        }
    }

    /// Clears all filters that have not been polled for longer than the configured
    /// `stale_filter_ttl` at the given instant.
    pub async fn clear_stale_filters(&self, now: Instant) {
        self.active_filters.retain(|id, filter| {
            let is_valid = (now - filter.last_poll_timestamp) < self.stale_filter_ttl;

            if !is_valid {
                tracing::trace!(?id, "evicting stale filter");
            }

            is_valid
        })
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
impl<RpcStorage: ReadRpcStorage, Mempool: L2TransactionPool> EthFilterApiServer
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
        if self.active_filters.remove(&id).is_some() {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        Ok(self.logs_impl(filter).to_rpc_result()?)
    }
}

/// An active installed filter
#[derive(Debug)]
struct ActiveFilter {
    /// At which block the filter was polled last.
    block: u64,
    /// Last time this filter was polled.
    last_poll_timestamp: Instant,
    /// What kind of filter it is.
    kind: FilterKind,
}

#[derive(Clone, Debug)]
enum FilterKind {
    Log(Box<Filter>),
    Block,
    PendingTransaction(PendingTransactionKind),
}

/// Represents the kind of pending transaction data that can be retrieved.
///
/// This enum differentiates between two kinds of pending transaction data:
/// - Just the transaction hashes.
/// - Full transaction details.
#[derive(Debug, Clone)]
enum PendingTransactionKind {
    Hashes(PendingTransactionsReceiver),
    FullTransaction(FullTransactionsReceiver),
}

impl PendingTransactionKind {
    async fn drain(&self) -> FilterChanges<Transaction<L2Envelope>> {
        match self {
            Self::Hashes(receiver) => receiver.drain().await,
            Self::FullTransaction(receiver) => receiver.drain().await,
        }
    }
}

/// A receiver for pending transactions that returns all new transactions since the last poll.
#[derive(Debug, Clone)]
struct PendingTransactionsReceiver {
    receiver: Arc<Mutex<mpsc::Receiver<TxHash>>>,
}

impl PendingTransactionsReceiver {
    fn new(receiver: mpsc::Receiver<TxHash>) -> Self {
        Self {
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }

    /// Returns all new pending transactions received since the last poll.
    async fn drain(&self) -> FilterChanges<Transaction<L2Envelope>> {
        let mut pending_txs = Vec::new();
        let mut prepared_stream = self.receiver.lock().await;

        while let Ok(tx_hash) = prepared_stream.try_recv() {
            pending_txs.push(tx_hash);
        }

        // Convert the vector of hashes into FilterChanges::Hashes
        FilterChanges::Hashes(pending_txs)
    }
}

/// A structure to manage and provide access to a stream of full transaction details.
#[derive(Debug, Clone)]
struct FullTransactionsReceiver {
    txs_stream: Arc<Mutex<NewSubpoolTransactionStream<L2PooledTransaction>>>,
}

impl FullTransactionsReceiver {
    fn new(txs_stream: NewSubpoolTransactionStream<L2PooledTransaction>) -> Self {
        Self {
            txs_stream: Arc::new(Mutex::new(txs_stream)),
        }
    }

    /// Returns all new pending transactions received since the last poll.
    async fn drain(&self) -> FilterChanges<Transaction<L2Envelope>> {
        let mut pending_txs = Vec::new();
        let mut prepared_stream = self.txs_stream.lock().await;

        while let Ok(tx) = prepared_stream.try_recv() {
            let (tx, signer) = tx.transaction.to_consensus().into_parts();
            let tx = L2Envelope::from(tx);
            pending_txs.push(Transaction::from_transaction(
                Recovered::new_unchecked(tx, signer),
                TransactionInfo::default(),
            ));
        }
        FilterChanges::Transactions(pending_txs)
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
