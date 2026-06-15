use crate::cache::{TreeBlockCache, TreeBlockCacheReceiverExt};
use alloy::primitives::B256;
use anyhow::Context;
use std::ops::RangeInclusive;
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use zksync_os_batch_types::{DiscoveredCommittedBatch, ExtendedCommitBatchInfo};
use zksync_os_types::PubdataMode;

/// An L1-committed batch to check, built purely from `BlockCommit` + `ReportCommittedBatchRange`
/// events — no commit calldata. The two hashes are all we need to validate a local rebuild:
/// `commitment` (the batch output hash) binds every other `StoredBatchInfo` field, and
/// `state_commitment` is the post-batch state root.
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

/// Checks L1-committed batches against locally replayed blocks by rebuilding each batch's
/// commitment from the local block cache and matching it against the on-chain event hashes.
///
/// The rebuilt [`StoredBatchInfo`](zksync_os_contract_interface::models::StoredBatchInfo) — which,
/// once matched, is provably identical to what L1 committed — is sent back to the persist batch
/// watcher so it can be persisted without ever fetching commit calldata.
///
/// Verification is concurrent; verified batches are emitted as soon as they complete (the persist
/// watcher orders them via execute events).
pub struct L1ConsistencyChecker {
    chain_id: u64,
    sl_chain_id: u64,
    last_persisted_block_on_start: u64,
    /// Shared with the cacher and batch verification responder.
    cache: watch::Sender<TreeBlockCache>,
    cache_rx: watch::Receiver<TreeBlockCache>,
    /// Pubdata-mode candidates to try when rebuilding, cheapest first. More than one only when the
    /// DA scheme can't be derived from settlement config alone (L1 + Rollup: Calldata vs Blobs);
    /// the candidate whose rebuilt `commitment` matches the event identifies the scheme.
    da_candidates: Arc<Vec<PubdataMode>>,
    /// Verified batches, sent back to the persist batch watcher for persistence.
    verified_batch_tx: mpsc::Sender<DiscoveredCommittedBatch>,
    l1_events_rx: mpsc::Receiver<L1CommittedBatch>,
    verification_concurrency: usize,
}

impl L1ConsistencyChecker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain_id: u64,
        sl_chain_id: u64,
        last_persisted_block_on_start: u64,
        cache: watch::Sender<TreeBlockCache>,
        da_candidates: Vec<PubdataMode>,
        verified_batch_tx: mpsc::Sender<DiscoveredCommittedBatch>,
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
            da_candidates: Arc::new(da_candidates),
            verified_batch_tx,
            l1_events_rx,
            verification_concurrency,
        }
    }

    /// Rebuilds one committed batch from its cached blocks and matches it against the event hashes,
    /// returning the verified local [`StoredBatchInfo`](zksync_os_contract_interface::models::StoredBatchInfo).
    async fn verify_commit(
        cache_rx: watch::Receiver<TreeBlockCache>,
        chain_id: u64,
        sl_chain_id: u64,
        da_candidates: Arc<Vec<PubdataMode>>,
        commit: L1CommittedBatch,
    ) -> anyhow::Result<DiscoveredCommittedBatch> {
        let batch_number = commit.batch_number;
        let range = commit.range.clone();
        let blocks = cache_rx
            .wait_for_range(range.clone())
            .await
            .context("while waiting for a committed batch's blocks to be cached")?;
        let state_commitment = commit.state_commitment;
        let commitment = commit.commitment;

        let batch_info = tokio::task::spawn_blocking(move || {
            // Try each DA-scheme candidate; a `commitment` match both verifies the batch and
            // identifies the scheme it was committed under.
            for &pubdata_mode in da_candidates.iter() {
                let (local_batch_info, _) = ExtendedCommitBatchInfo::build(
                    &blocks,
                    chain_id,
                    batch_number,
                    pubdata_mode,
                    sl_chain_id,
                );
                let local_stored = local_batch_info.into_stored();
                if local_stored.state_commitment == state_commitment
                    && local_stored.commitment == commitment
                {
                    return Ok(local_stored);
                }
            }
            tracing::error!(
                batch_number,
                ?state_commitment,
                ?commitment,
                "L1 committed batch is inconsistent with locally replayed blocks (no DA-scheme candidate matched)",
            );
            anyhow::bail!(
                "L1 committed batch #{batch_number} is inconsistent with locally replayed blocks"
            )
        })
        .await
        .context("while rebuilding a committed batch's commitment")??;

        tracing::info!(
            "verified L1 committed batch #{} against locally replayed blocks {:?}",
            batch_number,
            range,
        );
        Ok(DiscoveredCommittedBatch {
            batch_info,
            block_range: range,
        })
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
                    // Batches already persisted before startup were verified by a previous run;
                    // their blocks are not cached, so don't wait for them.
                    if commit.last_block_number() <= self.last_persisted_block_on_start {
                        continue;
                    }
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
                    let da_candidates = self.da_candidates.clone();
                    tasks.spawn(async move {
                        let _permit = permit; // held until the verification finishes
                        Self::verify_commit(cache_rx, chain_id, sl_chain_id, da_candidates, commit)
                            .await
                    });
                }
                Some(joined) = tasks.join_next() => {
                    let verified = joined.context("verification task panicked")??;
                    self.handle_verified(verified).await?;
                }
            }
        }

        while let Some(joined) = tasks.join_next().await {
            let verified = joined.context("verification task panicked")??;
            self.handle_verified(verified).await?;
        }
        Ok(())
    }

    /// Evicts the verified batch's blocks from the cache and hands the rebuilt batch back to the
    /// persist watcher.
    async fn handle_verified(&self, verified: DiscoveredCommittedBatch) -> anyhow::Result<()> {
        self.cache
            .send_modify(|cache| cache.remove_range(verified.block_range.clone()));
        self.verified_batch_tx
            .send(verified)
            .await
            .context("persist batch watcher closed the verified-batch channel")?;
        Ok(())
    }
}
