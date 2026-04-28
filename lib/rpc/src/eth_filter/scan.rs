use super::EthFilterError;
use crate::eth_impl::build_api_log;
use crate::metrics::{FilterCategory, GET_LOGS, GetLogsStat};
use alloy::rpc::types::{Filter, Log};
use zksync_os_storage::log_index_filter::candidates;
use zksync_os_storage_api::{ReadRepository, RepositoryBlock, StoredTxData};

type EthFilterResult<T> = Result<T, EthFilterError>;

/// Scans blocks in `from..=to` and returns matching logs.
///
/// Uses the log index (if available) to skip blocks that cannot contain matching logs. Blocks
/// outside the index's covered range fall back to per-block bloom filter checks.
pub(crate) fn scan_logs(
    repo: &dyn ReadRepository,
    filter: &Filter,
    from: u64,
    to: u64,
    max_logs: Option<usize>,
) -> EthFilterResult<Vec<Log>> {
    let candidates = candidates(repo, filter, from..to + 1)?;

    let is_multi_block_range = from != to;
    let mut stats = BlockScanStats::new(to - from + 1, filter, candidates.covered_len());
    let mut logs = Vec::new();

    for number in from..=to {
        if !candidates.may_contain(number) {
            stats.skipped_by_index += 1;
            continue;
        }

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
                stats.bloom_true_positive += 1;
            } else {
                stats.bloom_false_positive += 1;
            }

            // size check but only if range is multiple blocks, so we always return all
            // logs of a single block
            if let Some(max_logs) = max_logs
                && is_multi_block_range
                && logs.len() > max_logs
            {
                stats.truncated = true;
                return Err(EthFilterError::QueryExceedsMaxResults {
                    max_logs,
                    from_block: from,
                    to_block: number.saturating_sub(1),
                });
            }
        } else {
            stats.bloom_negative += 1;
        }
    }

    stats.logs_returned = logs.len() as u64;
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
    covered_len: u64,
    skipped_by_index: u64,
    bloom_true_positive: u64,
    bloom_false_positive: u64,
    bloom_negative: u64,
    logs_returned: u64,
    category: FilterCategory,
    truncated: bool,
}

impl BlockScanStats {
    fn new(total: u64, filter: &Filter, covered_len: u64) -> Self {
        Self {
            total,
            covered_len,
            skipped_by_index: 0,
            bloom_true_positive: 0,
            bloom_false_positive: 0,
            bloom_negative: 0,
            logs_returned: 0,
            category: FilterCategory::from(filter),
            truncated: false,
        }
    }
}

impl Drop for BlockScanStats {
    fn drop(&mut self) {
        let cat = self.category;
        GET_LOGS.scanned_blocks[&(GetLogsStat::Total, cat)].observe(self.total);
        GET_LOGS.scanned_blocks[&(GetLogsStat::SkippedByIndex, cat)].observe(self.skipped_by_index);
        GET_LOGS.scanned_blocks[&(GetLogsStat::BloomTruePositive, cat)]
            .observe(self.bloom_true_positive);
        GET_LOGS.scanned_blocks[&(GetLogsStat::BloomFalsePositive, cat)]
            .observe(self.bloom_false_positive);
        GET_LOGS.scanned_blocks[&(GetLogsStat::BloomNegative, cat)].observe(self.bloom_negative);
        GET_LOGS.scanned_blocks[&(GetLogsStat::LogsReturned, cat)].observe(self.logs_returned);
        if self.total > 0 {
            GET_LOGS.index_skip_ratio[&cat]
                .observe(self.skipped_by_index as f64 / self.total as f64);
            GET_LOGS.index_coverage[&cat].observe(self.covered_len as f64 / self.total as f64);
        }
        let bloom_checked = self.bloom_true_positive + self.bloom_false_positive;
        if bloom_checked > 0 {
            GET_LOGS.bloom_precision[&cat]
                .observe(self.bloom_true_positive as f64 / bloom_checked as f64);
        }
        if self.truncated {
            GET_LOGS.truncated[&cat].inc();
        }
    }
}
