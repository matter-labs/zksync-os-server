use super::{resolve_block_id, EthNamespace};
use crate::api::metrics::API_METRICS;
use alloy::eips::BlockId;
use alloy::primitives::Bloom;
use alloy::rpc::types::{Filter, FilterBlockOption, Log};
use jsonrpsee::core::RpcResult;

impl EthNamespace {
    pub async fn logs_impl(&self, filter: Filter) -> RpcResult<Vec<Log>> {
        let latency = API_METRICS.response_time[&"get_logs"].start();

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

        let mut logs = Vec::new();
        for number in from..=to {
            if let Some(block) = self
                .repository_manager
                .block_receipt_repository
                .get_by_number(number)
            {
                let block_bloom = Bloom::new(block.header.logs_bloom);
                if filter.matches_bloom(block_bloom) {
                    let receipts = self
                        .repository_manager
                        .transaction_receipt_repository
                        .get_by_block_number(number);
                    for tx_data in receipts {
                        for log in tx_data.receipt.logs() {
                            if filter.matches(&log.inner) {
                                logs.push(log.clone());
                            }
                        }
                    }
                }
            }
        }

        latency.observe();
        Ok(logs)
    }
}
