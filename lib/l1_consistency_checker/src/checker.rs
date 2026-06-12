use crate::cache::{TreeBlockCache, TreeBlockCacheReceiverExt};
use anyhow::Context;
use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;
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

/// Outcome of verifying one committed batch, carried back to the run loop so it can evict the
/// batch's blocks and advance the verified-batch watermark.
struct VerifiedBatch {
    batch_number: u64,
    range: RangeInclusive<u64>,
}

/// Background task that checks L1-committed batches against locally replayed blocks.
///
/// It runs off the pipeline (unlike the [`LocalBatchDataCacher`](crate::cacher::LocalBatchDataCacher)
/// that fills the cache) so that its CPU-heavy commitment rebuild cannot starve block intake.
/// Committed batches arrive in order, but each is verified in its own bounded-concurrency task so
/// that one batch waiting for its blocks (or rebuilding its commitment) does not hold up batches
/// behind it. As each batch is verified its blocks are evicted from the shared cache — restoring
/// intake capacity — and the latest-verified-batch watermark is advanced (see [`Self::run`] for
/// why the watermark only moves along the contiguous verified prefix).
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
    /// Upper bound on batches verified concurrently (and thus on parallel commitment rebuilds).
    verification_concurrency: usize,
}

impl L1ConsistencyChecker {
    pub fn new(
        chain_id: u64,
        sl_chain_id: u64,
        last_persisted_block_on_start: u64,
        cache: watch::Sender<TreeBlockCache>,
        latest_verified_batch_tx: watch::Sender<u64>,
        l1_events_rx: mpsc::Receiver<L1CommittedBatch>,
        verification_concurrency: usize,
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
            verification_concurrency,
        }
    }

    /// Verifies a single committed batch against locally replayed blocks, blocking until the
    /// cacher has the batch's blocks available. Batches already covered by a persisted batch were
    /// verified by a previous run and are trusted without rebuilding.
    ///
    /// Takes everything by value so it can run as an independent task; the CPU-heavy commitment
    /// rebuild is offloaded to a blocking thread so it parallelizes across cores.
    async fn verify_commit(
        cache_rx: watch::Receiver<TreeBlockCache>,
        chain_id: u64,
        sl_chain_id: u64,
        last_persisted_block_on_start: u64,
        commit: L1CommittedBatch,
    ) -> anyhow::Result<VerifiedBatch> {
        let batch_number = commit.batch_number();
        let range = commit.range.clone();

        if commit.last_block_number() > last_persisted_block_on_start {
            let blocks = cache_rx
                .wait_for_range(range.clone())
                .await
                .context("while waiting for a committed batch's blocks to be cached")?;
            let l2_da_commitment_scheme = commit.l2_da_commitment_scheme;
            let l1_stored = commit.stored_batch_info;

            tokio::task::spawn_blocking(move || {
                let (local_batch_info, _) = ExtendedCommitBatchInfo::build(
                    &blocks,
                    chain_id,
                    batch_number,
                    PubdataMode::from_da_commitment_scheme(l2_da_commitment_scheme),
                    sl_chain_id,
                );

                let local_stored = local_batch_info.into_stored();
                if local_stored != l1_stored {
                    tracing::error!(
                        "L1 committed batch #{} is inconsistent with locally replayed blocks, expected: {:?}, received: {:?}",
                        batch_number,
                        local_stored,
                        l1_stored,
                    );
                    anyhow::bail!(
                        "L1 committed batch #{} is inconsistent with locally replayed blocks",
                        batch_number
                    );
                }
                Ok(())
            })
            .await
            .context("while rebuilding a committed batch's commitment")??;

            tracing::info!(
                "verified L1 committed batch #{} against locally replayed blocks {:?}",
                batch_number,
                range,
            );
        }

        Ok(VerifiedBatch {
            batch_number,
            range,
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        tracing::info!("starting L1 consistency checker");

        let semaphore = Arc::new(Semaphore::new(self.verification_concurrency));
        let mut tasks: JoinSet<anyhow::Result<VerifiedBatch>> = JoinSet::new();

        // Verifications complete out of order, but downstream (the persist batch watcher) must not
        // see batch N marked verified before batch N-1 is. We therefore stage verified batch
        // numbers and only advance the published watermark across the contiguous prefix that is
        // fully verified. Eviction has no such constraint — a batch's blocks belong to it alone,
        // so they are dropped as soon as that batch is verified, regardless of order.
        let mut verified_ahead: BTreeSet<u64> = BTreeSet::new();
        let mut next_batch_to_confirm = *self.latest_verified_batch_tx.borrow() + 1;

        loop {
            tokio::select! {
                maybe_commit = self.l1_events_rx.recv() => {
                    let Some(commit) = maybe_commit else {
                        // Channel closed: stop accepting work and drain what is in flight.
                        break;
                    };
                    tracing::debug!(
                        "received L1 committed batch {} for consistency checking in range {:?}",
                        commit.batch_number(),
                        commit.range,
                    );
                    // Acquiring the permit here bounds in-flight verifications, providing
                    // backpressure on intake of new commits.
                    let permit = semaphore
                        .clone()
                        .acquire_owned()
                        .await
                        .expect("verification semaphore is never closed");
                    let cache_rx = self.cache_rx.clone();
                    let chain_id = self.chain_id;
                    let sl_chain_id = self.sl_chain_id;
                    let last_persisted_block_on_start = self.last_persisted_block_on_start;
                    tasks.spawn(async move {
                        let _permit = permit; // held until the verification finishes
                        Self::verify_commit(
                            cache_rx,
                            chain_id,
                            sl_chain_id,
                            last_persisted_block_on_start,
                            commit,
                        )
                        .await
                    });
                }
                Some(joined) = tasks.join_next() => {
                    let verified = joined.context("verification task panicked")??;
                    self.handle_verified(verified, &mut verified_ahead, &mut next_batch_to_confirm);
                }
            }
        }

        // Finish the verifications that were still in flight when the channel closed.
        while let Some(joined) = tasks.join_next().await {
            let verified = joined.context("verification task panicked")??;
            self.handle_verified(verified, &mut verified_ahead, &mut next_batch_to_confirm);
        }
        Ok(())
    }

    /// Applies one completed verification: evicts the batch's now-redundant blocks and advances the
    /// verified-batch watermark across the contiguous prefix that is fully verified.
    fn handle_verified(
        &self,
        verified: VerifiedBatch,
        verified_ahead: &mut BTreeSet<u64>,
        next_batch_to_confirm: &mut u64,
    ) {
        // Drop the now-verified blocks from the cache, restoring intake capacity for the cacher.
        self.cache
            .send_modify(|cache| cache.remove_range(verified.range));

        // Batches at or below the current watermark were already verified on a previous run; only
        // track the ones that can move it forward.
        if verified.batch_number >= *next_batch_to_confirm {
            verified_ahead.insert(verified.batch_number);
        }
        while verified_ahead.remove(next_batch_to_confirm) {
            *next_batch_to_confirm += 1;
        }

        let confirmed = *next_batch_to_confirm - 1;
        self.latest_verified_batch_tx.send_if_modified(|latest| {
            if confirmed > *latest {
                *latest = confirmed;
                true
            } else {
                false
            }
        });
    }
}
