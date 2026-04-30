use crate::util;
use crate::watcher::L1Watcher;
use crate::{L1WatcherConfig, ProcessRawEvents};
use alloy::providers::DynProvider;
use std::collections::VecDeque;
use zksync_os_contract_interface::ZkChain;

/// Description of a single settlement-layer segment that [`SlAwareL1Watcher`] should scan, in
/// isolation, before advancing to the next one. `last_batch = None` marks the open-ended (live)
/// segment; it must appear exactly once, as the final entry.
#[derive(Clone, Debug)]
pub struct SegmentSpec {
    /// ZKChain (diamond proxy + provider) that hosts commit/execute events for this segment.
    pub zk_chain: ZkChain<DynProvider>,
    /// First batch, inclusive, whose commit block the watcher should resolve to its scan start.
    pub first_batch: u64,
    /// Last batch, inclusive, whose execute block closes the segment. `None` means open-ended.
    pub last_batch: Option<u64>,
}

/// Settlement-layer-aware variant of [`L1Watcher`] that walks a chain of SL segments
/// (L1 → Gateway → L1 → …) in order. Historical segments are scanned to completion once their
/// commit and execute blocks resolve; the final open-ended segment is tailed live against the
/// finalized boundary so events that haven't yet been irreversibly observed on-chain are not
/// processed.
pub struct SlAwareL1Watcher {
    config: L1WatcherConfig,
    segments: VecDeque<SegmentSpec>,
    processor: Box<dyn ProcessRawEvents>,
}

impl SlAwareL1Watcher {
    pub fn new(
        config: L1WatcherConfig,
        segments: Vec<SegmentSpec>,
        processor: Box<dyn ProcessRawEvents>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !segments.is_empty(),
            "SlAwareL1Watcher requires at least one segment"
        );
        anyhow::ensure!(
            segments.last().unwrap().last_batch.is_none(),
            "SlAwareL1Watcher requires the final segment to be open-ended (`last_batch = None`)"
        );
        for seg in &segments[..segments.len() - 1] {
            anyhow::ensure!(
                seg.last_batch.is_some(),
                "non-final SlAwareL1Watcher segments must be closed"
            );
        }

        Ok(Self {
            config,
            segments: segments.into(),
            processor,
        })
    }

    pub async fn run(self) {
        let Self {
            config,
            mut segments,
            mut processor,
        } = self;
        while let Some(segment) = segments.pop_front() {
            processor = match run_segment(config.clone(), segment, processor).await {
                Ok(processor) => processor,
                Err(e) => {
                    tracing::error!("sl-aware l1 watcher fatal error: {e}");
                    panic!("sl-aware watcher failed: {e}");
                }
            };
        }
        // Unreachable: the constructor enforces a non-empty segment list whose final entry is
        // open-ended, so the loop above never exits normally.
    }
}

async fn run_segment(
    config: L1WatcherConfig,
    segment: SegmentSpec,
    processor: Box<dyn ProcessRawEvents>,
) -> anyhow::Result<Box<dyn ProcessRawEvents>> {
    let zk_chain = segment.zk_chain.clone();

    let start_block = util::find_l1_commit_block_by_batch_number(
        zk_chain.clone(),
        segment.first_batch,
        config.max_blocks_to_process,
    )
    .await?;

    let end_block = match segment.last_batch {
        Some(lb) => Some(util::find_l1_execute_block_by_batch_number(zk_chain.clone(), lb).await?),
        None => None,
    };

    tracing::info!(
        "sl-aware watcher activated segment at {} for batches=({}-{}), L1 blocks=({}-{})",
        zk_chain.address(),
        segment.first_batch,
        segment.last_batch.unwrap_or(u64::MAX),
        start_block,
        end_block.unwrap_or(u64::MAX),
    );

    // Closed segments are bounded by an executed batch on-chain, so the boundary mode does not
    // matter — `end_block` dominates the cap. The open-ended segment uses the finalized boundary
    // so persistence-style processors only react to irreversibly observed events.
    let mut watcher = L1Watcher::new_finalized(
        config,
        zk_chain.provider().clone(),
        (*zk_chain.address()).into(),
        start_block,
        end_block,
        processor,
    );
    watcher.run_inner().await;
    Ok(watcher.processor)
}
