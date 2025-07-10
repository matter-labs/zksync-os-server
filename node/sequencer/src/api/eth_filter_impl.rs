use super::resolve_block_id;
use crate::api::metrics::API_METRICS;
use crate::api::types::QueryLimits;
use crate::config::RpcConfig;
use crate::finality::FinalityTracker;
use crate::repositories::RepositoryManager;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::rpc::types::{
    Filter, FilterBlockOption, FilterChanges, FilterId, Log, PendingTransactionFilterKind,
};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use zksync_os_rpc_api::filter::EthFilterApiServer;

pub(crate) struct EthFilterNamespace {
    pub(super) repository_manager: RepositoryManager,
    pub(super) finality_info: FinalityTracker,
    pub(super) query_limits: QueryLimits,
}

impl EthFilterNamespace {
    pub fn new(
        config: RpcConfig,
        repository_manager: RepositoryManager,
        finality_tracker: FinalityTracker,
    ) -> Self {
        let query_limits =
            QueryLimits::new(config.max_blocks_per_filter, config.max_logs_per_response);
        Self {
            repository_manager,
            finality_info: finality_tracker,
            query_limits,
        }
    }
}

#[async_trait]
impl EthFilterApiServer<()> for EthFilterNamespace {
    async fn new_filter(&self, _filter: Filter) -> RpcResult<FilterId> {
        todo!()
    }

    async fn new_block_filter(&self) -> RpcResult<FilterId> {
        todo!()
    }

    async fn new_pending_transaction_filter(
        &self,
        _kind: Option<PendingTransactionFilterKind>,
    ) -> RpcResult<FilterId> {
        todo!()
    }

    async fn filter_changes(&self, _id: FilterId) -> RpcResult<FilterChanges<()>> {
        todo!()
    }

    async fn filter_logs(&self, _id: FilterId) -> RpcResult<Vec<Log>> {
        todo!()
    }

    async fn uninstall_filter(&self, _id: FilterId) -> RpcResult<bool> {
        todo!()
    }

    async fn logs(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        let latency = API_METRICS.response_time[&"get_logs"].start();
        let (from, to) = match filter.block_option {
            FilterBlockOption::AtBlockHash(block_hash) => {
                let block = resolve_block_id(BlockId::Hash(block_hash.into()), &self.finality_info);
                (block, block)
            }
            FilterBlockOption::Range {
                from_block,
                to_block,
            } => (
                resolve_block_id(
                    from_block
                        .map(BlockId::Number)
                        .unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)),
                    &self.finality_info,
                ),
                resolve_block_id(
                    to_block
                        .map(BlockId::Number)
                        .unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)),
                    &self.finality_info,
                ),
            ),
        };
        tracing::trace!(from, to, ?filter, "Processing eth_getLogs request");

        if let Some(max_blocks_per_filter) = self
            .query_limits
            .max_blocks_per_filter
            .filter(|limit| to - from > *limit)
        {
            let message = format!("query exceeds max block range {max_blocks_per_filter}");
            return Err(ErrorObjectOwned::owned(
                jsonrpsee::types::error::INVALID_PARAMS_CODE,
                message,
                None::<()>,
            ));
        }

        let is_multi_block_range = from != to;
        let total_scanned_blocks = to - from + 1;
        let mut tp_scanned_blocks = 0u64;
        let mut fp_scanned_blocks = 0u64;
        let mut negative_scanned_blocks = 0u64;
        let mut logs = Vec::new();
        for number in from..=to {
            if let Some((header, tx_hashes)) = self.repository_manager.get_block_by_number(number) {
                if filter.matches_bloom(header.inner.logs_bloom) {
                    tracing::trace!(
                        number,
                        ?filter,
                        "Block matches bloom filter, scanning receipts",
                    );
                    let tx_receipts = tx_hashes.into_iter().map(|hash| {
                        self.repository_manager
                            .get_tx_by_hash(hash)
                            .unwrap_or_else(|| {
                                panic!("Missing tx receipt for hash: {hash:?} in block {number}")
                            })
                    });
                    let mut at_least_one_log_added = false;
                    for tx_data in tx_receipts {
                        for log in tx_data.receipt.logs() {
                            if filter.matches(&log.inner) {
                                logs.push(log.clone());
                                at_least_one_log_added = true;
                            }
                        }
                    }
                    if at_least_one_log_added {
                        tp_scanned_blocks += 1;
                    } else {
                        fp_scanned_blocks += 1;
                    }

                    // size check but only if range is multiple blocks, so we always return all
                    // logs of a single block
                    if let Some(max_logs_per_response) = self.query_limits.max_logs_per_response {
                        if is_multi_block_range && logs.len() > max_logs_per_response {
                            let suggested_to = number.saturating_sub(1);
                            let message = format!(
                                "query exceeds max results {}, retry with the range {}-{}",
                                max_logs_per_response, from, suggested_to
                            );
                            return Err(ErrorObjectOwned::owned(
                                jsonrpsee::types::error::INVALID_PARAMS_CODE,
                                message,
                                None::<()>,
                            ));
                        }
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
        latency.observe();

        Ok(logs)
    }
}
