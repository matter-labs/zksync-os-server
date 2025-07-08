use super::{resolve_block_id, EthNamespace};
use crate::api::metrics::API_METRICS;
use alloy::eips::BlockId;
use alloy::primitives::Bloom;
use alloy::rpc::types::{Filter, FilterBlockOption, Log};
use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use zk_ee::utils::Bytes32;

impl EthNamespace {
    pub async fn logs_impl(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        let (from, to) = match filter.block_option {
            FilterBlockOption::AtBlockHash(block_hash) => {
                let block =
                    resolve_block_id(Some(BlockId::Hash(block_hash.into())), &self.finality_info);
                (block, block)
            }
            FilterBlockOption::Range {
                from_block,
                to_block,
            } => (
                resolve_block_id(from_block.map(BlockId::Number), &self.finality_info),
                resolve_block_id(to_block.map(BlockId::Number), &self.finality_info),
            ),
        };
        tracing::trace!(
            "Processing eth_getLogs request with filter: {:?}, from: {}, to: {}",
            filter,
            from,
            to
        );

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
            if let Some((block, tx_hashes)) = self
                .repository_manager
                .block_receipt_repository
                .get_by_number(number)
            {
                let block_bloom = Bloom::new(block.header.logs_bloom);
                if filter.matches_bloom(block_bloom) {
                    tracing::trace!(
                        "Block {} matches bloom filter {:?}, scanning receipts",
                        number,
                        filter
                    );
                    let tx_receipts = tx_hashes.into_iter().map(|hash| {
                        self.repository_manager
                            .transaction_receipt_repository
                            .get_by_hash(&Bytes32::from(hash.0))
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

        Ok(logs)
    }
}
