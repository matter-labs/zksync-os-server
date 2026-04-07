use super::EthFilterError;
use crate::eth_impl::build_api_log;
use crate::metrics::API_METRICS;
use alloy::rpc::types::{Filter, Log};
use zksync_os_storage_api::{ReadRepository, RepositoryBlock, StoredTxData};

type EthFilterResult<T> = Result<T, EthFilterError>;

/// Scans blocks in `from..=to` using per-block bloom filters and returns matching logs.
///
/// This is the naive O(block_range) implementation. It will be replaced by an inverted index
/// lookup once the index is implemented.
pub(crate) fn scan_logs(
    repo: &dyn ReadRepository,
    filter: &Filter,
    from: u64,
    to: u64,
    max_logs: Option<usize>,
) -> EthFilterResult<Vec<Log>> {
    let is_multi_block_range = from != to;
    let mut stats = BlockScanStats::new(to - from + 1);
    let mut logs = Vec::new();

    for number in from..=to {
        let Some(block) = repo.get_block_by_number(number)? else {
            return Err(EthFilterError::BlockNotFound(number.into()));
        };
        if filter.matches_bloom(block.header.logs_bloom) {
            tracing::trace!(
                number,
                ?filter,
                "Block matches bloom filter, scanning receipts"
            );
            let stored_txs = get_block_transactions(repo, block, number)?;
            let logs_before = logs.len();
            collect_matching_logs(filter, stored_txs, &mut logs);
            if logs.len() > logs_before {
                stats.true_positive += 1;
            } else {
                stats.false_positive += 1;
            }

            // size check but only if range is multiple blocks, so we always return all
            // logs of a single block
            if let Some(max_logs) = max_logs
                && is_multi_block_range
                && logs.len() > max_logs
            {
                return Err(EthFilterError::QueryExceedsMaxResults {
                    max_logs,
                    from_block: from,
                    to_block: number.saturating_sub(1),
                });
            }
        } else {
            stats.negative += 1;
        }
    }

    Ok(logs)
}

fn get_block_transactions(
    repo: &dyn ReadRepository,
    block: RepositoryBlock,
    block_number: u64,
) -> EthFilterResult<Vec<StoredTxData>> {
    block
        .unseal()
        .body
        .transactions
        .into_iter()
        .map(|hash| {
            repo.get_stored_transaction(hash)?
                .ok_or(EthFilterError::BlockNotFound(block_number.into()))
        })
        .collect()
}

fn collect_matching_logs(filter: &Filter, stored_txs: Vec<StoredTxData>, out: &mut Vec<Log>) {
    let mut log_index_in_block = 0u64;
    for tx in stored_txs {
        for inner_log in tx.receipt.logs() {
            if filter.matches(inner_log) {
                out.push(build_api_log(
                    *tx.tx.hash(),
                    inner_log.clone(),
                    tx.meta.clone(),
                    log_index_in_block - tx.meta.number_of_logs_before_this_tx,
                ));
            }
            log_index_in_block += 1;
        }
    }
}

/// Tracks bloom filter scan statistics for a single `eth_getLogs` call.
/// Observes Prometheus metrics when dropped, ensuring they are recorded on all exit paths.
struct BlockScanStats {
    total: u64,
    true_positive: u64,
    false_positive: u64,
    negative: u64,
}

impl BlockScanStats {
    fn new(total: u64) -> Self {
        Self {
            total,
            true_positive: 0,
            false_positive: 0,
            negative: 0,
        }
    }
}

impl Drop for BlockScanStats {
    fn drop(&mut self) {
        API_METRICS.get_logs_scanned_blocks[&"total"].observe(self.total);
        API_METRICS.get_logs_scanned_blocks[&"true_positive"].observe(self.true_positive);
        API_METRICS.get_logs_scanned_blocks[&"false_positive"].observe(self.false_positive);
        API_METRICS.get_logs_scanned_blocks[&"negative"].observe(self.negative);
    }
}
