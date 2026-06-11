use crate::cache::{TreeBlockCache, TreeBlockCacheReceiverExt};
use std::ops::RangeInclusive;
use tokio::sync::{mpsc, watch};
use zksync_os_batch_types::ExtendedCommitBatchInfo;
use zksync_os_contract_interface::models::{DACommitmentScheme, StoredBatchInfo};
use zksync_os_types::PubdataMode;

pub struct L1CommittedBatch {
    pub stored_batch_info: StoredBatchInfo,
    pub l2_da_commitment_scheme: DACommitmentScheme,
    pub range: RangeInclusive<u64>,
}

impl L1CommittedBatch {
    pub fn batch_number(&self) -> u64 {
        self.stored_batch_info.batch_number
    }

    pub fn last_block_number(&self) -> u64 {
        *self.range.end()
    }
}

/// Background task that checks L1-committed batches against locally replayed blocks.
///
/// It runs off the pipeline (unlike the [`LocalBatchDataCacher`](crate::cacher::LocalBatchDataCacher)
/// that fills the cache) so that its CPU-heavy commitment rebuild cannot starve block intake.
/// It consumes committed batches in order, waits until the cacher has folded the batch's blocks
/// into the shared cache, rebuilds the commitment locally, compares it against L1, then evicts the
/// verified prefix and advances the latest-verified-batch watermark.
pub struct L1ConsistencyChecker {
    chain_id: u64,
    sl_chain_id: u64,
    last_persisted_block_on_start: u64,
    /// Shared with the cacher (which inserts) and the batch verification responder (which reads).
    /// We hold the sender to evict verified blocks and a receiver to await freshly cached ones.
    cache: watch::Sender<TreeBlockCache>,
    cache_rx: watch::Receiver<TreeBlockCache>,
    latest_verified_batch_tx: watch::Sender<u64>,
    /// Receives L1-committed batches to verify against locally replayed blocks.
    l1_events_rx: mpsc::Receiver<L1CommittedBatch>,
}

impl L1ConsistencyChecker {
    pub fn new(
        chain_id: u64,
        sl_chain_id: u64,
        last_persisted_block_on_start: u64,
        cache: watch::Sender<TreeBlockCache>,
        latest_verified_batch_tx: watch::Sender<u64>,
        l1_events_rx: mpsc::Receiver<L1CommittedBatch>,
    ) -> Self {
        let cache_rx = cache.subscribe();
        Self {
            chain_id,
            sl_chain_id,
            last_persisted_block_on_start,
            cache,
            cache_rx,
            latest_verified_batch_tx,
            l1_events_rx,
        }
    }

    /// Verifies a single committed batch against locally replayed blocks, blocking until the
    /// cacher has the batch's blocks available. Batches already covered by a persisted batch were
    /// verified by a previous run and are trusted without rebuilding.
    async fn verify_commit(&self, commit: &L1CommittedBatch) -> anyhow::Result<()> {
        if commit.last_block_number() <= self.last_persisted_block_on_start {
            return Ok(());
        }

        let blocks = self.cache_rx.wait_for_range(commit.range.clone()).await?;

        let (local_batch_info, _) = ExtendedCommitBatchInfo::build(
            &blocks,
            self.chain_id,
            commit.batch_number(),
            PubdataMode::from_da_commitment_scheme(commit.l2_da_commitment_scheme),
            self.sl_chain_id,
        );

        let local_stored = local_batch_info.into_stored();
        let l1_stored = &commit.stored_batch_info;
        if &local_stored != l1_stored {
            tracing::error!(
                "L1 committed batch #{} is inconsistent with locally replayed blocks, expected: {:?}, received: {:?}",
                commit.batch_number(),
                local_stored,
                l1_stored,
            );
            anyhow::bail!(
                "L1 committed batch #{} is inconsistent with locally replayed blocks",
                commit.batch_number()
            );
        }

        Ok(())
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        tracing::info!("starting L1 consistency checker");
        // Committed batches arrive in order; the channel itself provides backpressure on the
        // L1 watcher, so we simply process them one at a time.
        while let Some(commit) = self.l1_events_rx.recv().await {
            tracing::debug!(
                "received L1 committed batch {} for consistency checking in range {:?}",
                commit.batch_number(),
                commit.range,
            );
            self.verify_commit(&commit).await?;
            tracing::info!(
                "verified L1 committed batch #{} against locally replayed blocks {:?}",
                commit.batch_number(),
                commit.range,
            );

            // Drop the now-verified prefix from the cache, restoring intake capacity for the
            // cacher, and publish the new watermark for the batch persist watcher.
            let last_block_number = commit.last_block_number();
            let batch_number = commit.batch_number();
            self.cache
                .send_modify(|cache| cache.remove_lower_or_equal_than(last_block_number));
            self.latest_verified_batch_tx.send_if_modified(|latest| {
                if batch_number > *latest {
                    *latest = batch_number;
                    true
                } else {
                    false
                }
            });
        }
        Ok(())
    }
}
