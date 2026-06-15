use crate::cache::{TreeBlockCache, TreeBlockCacheReceiverExt};
use alloy::primitives::B256;
use anyhow::Context;
use std::ops::RangeInclusive;
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use zksync_os_batch_types::{DiscoveredCommittedBatch, ExtendedCommitBatchInfo};
use zksync_os_types::PubdataMode;

pub struct L1CommittedBatch {
    pub batch_number: u64,
    pub state_commitment: B256,
    pub commitment: B256,
    pub range: RangeInclusive<u64>,
}

impl L1CommittedBatch {
    pub fn batch_number(&self) -> u64 {
        self.batch_number
    }

    pub fn last_block_number(&self) -> u64 {
        *self.range.end()
    }
}

/// Checks L1-committed batches against locally replayed blocks.
///
/// Verification is concurrent; reconstructed committed-batch data is sent back to the persist
/// watcher once a worker verifies it against local blocks.
pub struct L1ConsistencyChecker {
    chain_id: u64,
    sl_chain_id: u64,
    last_persisted_block_on_start: u64,
    /// Shared with the cacher and batch verification responder.
    cache: watch::Sender<TreeBlockCache>,
    cache_rx: watch::Receiver<TreeBlockCache>,
    l1_events_rx: mpsc::Receiver<L1CommittedBatch>,
    verified_batches_tx: mpsc::UnboundedSender<DiscoveredCommittedBatch>,
    verification_concurrency: usize,
}

impl L1ConsistencyChecker {
    pub fn new(
        chain_id: u64,
        sl_chain_id: u64,
        last_persisted_block_on_start: u64,
        cache: watch::Sender<TreeBlockCache>,
        l1_events_rx: mpsc::Receiver<L1CommittedBatch>,
        verified_batches_tx: mpsc::UnboundedSender<DiscoveredCommittedBatch>,
        verification_concurrency: usize,
    ) -> Self {
        let cache_rx = cache.subscribe();
        Self {
            chain_id,
            sl_chain_id,
            last_persisted_block_on_start,
            cache,
            cache_rx,
            l1_events_rx,
            verified_batches_tx,
            verification_concurrency,
        }
    }

    /// Verifies one committed batch once its local blocks are cached.
    async fn verify_commit(
        cache_rx: watch::Receiver<TreeBlockCache>,
        chain_id: u64,
        sl_chain_id: u64,
        last_persisted_block_on_start: u64,
        commit: L1CommittedBatch,
    ) -> anyhow::Result<DiscoveredCommittedBatch> {
        let batch_number = commit.batch_number();
        let range = commit.range.clone();

        anyhow::ensure!(
            commit.last_block_number() > last_persisted_block_on_start,
            "L1 committed batch #{} was already persisted on startup",
            batch_number,
        );

        let blocks = cache_rx
            .wait_for_range(range.clone())
            .await
            .context("while waiting for a committed batch's blocks to be cached")?;

        let verified = tokio::task::spawn_blocking(move || {
            for pubdata_mode in [
                PubdataMode::Calldata,
                PubdataMode::Validium,
                PubdataMode::Blobs,
            ] {
                let (local_batch_info, _) = ExtendedCommitBatchInfo::build(
                    &blocks,
                    chain_id,
                    batch_number,
                    pubdata_mode,
                    sl_chain_id,
                );
                let local_stored = local_batch_info.into_stored();
                if local_stored.state_commitment == commit.state_commitment
                    && local_stored.commitment == commit.commitment
                {
                    return Ok(DiscoveredCommittedBatch {
                        batch_info: local_stored,
                        block_range: range.clone(),
                    });
                }
            }

            tracing::error!(
                "L1 committed batch #{} is inconsistent with locally replayed blocks, state commitment {:?}, commitment {:?}",
                batch_number,
                commit.state_commitment,
                commit.commitment,
            );
            anyhow::bail!(
                "L1 committed batch #{} is inconsistent with locally replayed blocks",
                batch_number
            );
        })
        .await
        .context("while rebuilding a committed batch's commitment")??;

        tracing::info!(
            "verified L1 committed batch #{} against locally replayed blocks {:?}",
            batch_number,
            verified.block_range,
        );

        Ok(verified)
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        tracing::info!("starting L1 consistency checker");

        let semaphore = Arc::new(Semaphore::new(self.verification_concurrency));
        let mut tasks: JoinSet<anyhow::Result<DiscoveredCommittedBatch>> = JoinSet::new();

        loop {
            tokio::select! {
                maybe_commit = self.l1_events_rx.recv() => {
                    let Some(commit) = maybe_commit else {
                        break;
                    };
                    tracing::debug!(
                        "received L1 committed batch {} for consistency checking in range {:?}",
                        commit.batch_number(),
                        commit.range,
                    );
                    // Bound in-flight commitment rebuilds.
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
                    self.handle_verified(verified)?;
                }
            }
        }

        while let Some(joined) = tasks.join_next().await {
            let verified = joined.context("verification task panicked")??;
            self.handle_verified(verified)?;
        }
        Ok(())
    }

    /// Evicts verified blocks and publishes reconstructed batch data for persistence.
    fn handle_verified(&self, verified: DiscoveredCommittedBatch) -> anyhow::Result<()> {
        let range = verified.block_range.clone();
        self.cache.send_modify(|cache| cache.remove_range(range));

        self.verified_batches_tx
            .send(verified)
            .map_err(|_| anyhow::anyhow!("L1 persisted-batch watcher stopped"))?;
        Ok(())
    }
}
