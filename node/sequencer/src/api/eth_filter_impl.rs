use super::{resolve_block_id, EthNamespace};
use crate::api::metrics::API_METRICS;
use alloy::eips::BlockId;
use alloy::primitives::Bloom;
use alloy::rpc::types::{Filter, FilterBlockOption, Log};
use jsonrpsee::core::RpcResult;

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

        let total_scanned_blocks = to - from + 1;
        let mut tp_scanned_blocks = 0u64;
        let mut fp_scanned_blocks = 0u64;
        let mut negative_scanned_blocks = 0u64;
        let mut logs = Vec::new();
        for number in from..=to {
            if let Some(block) = self
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
                    let receipts = self
                        .repository_manager
                        .transaction_receipt_repository
                        .get_by_block_number(number);
                    let mut at_least_one_log_added = false;
                    for tx_data in receipts {
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
