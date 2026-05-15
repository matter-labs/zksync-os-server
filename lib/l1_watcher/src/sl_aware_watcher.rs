use crate::watcher::L1Watcher;
use crate::{BlockUpdates, L1WatcherConfig, ProcessRawEvents};
use alloy::primitives::{Address, BlockNumber};
use alloy::providers::DynProvider;
use alloy::rpc::types::ValueOrArray;
use std::collections::VecDeque;
use tokio::sync::watch;

/// Description of a single settlement-layer segment that [`SlAwareL1Watcher`] should scan, in
/// isolation, before advancing to the next one. `end_block = None` marks the open-ended (live)
/// segment; it must appear at most once, as the final entry.
///
/// Block boundaries are pre-resolved by the caller.
#[derive(Clone, Debug)]
pub struct SegmentSpec {
    /// Provider for the settlement layer this segment is scanned on.
    pub provider: DynProvider,
    /// Block updates for the segment's settlement-layer provider.
    pub block_updates: watch::Receiver<BlockUpdates>,
    /// Contract address(es) whose logs the segment scans (e.g. the chain's diamond proxy or a
    /// bridgehub's message-root contract).
    pub address: ValueOrArray<Address>,
    /// First SL block to scan from, inclusive.
    pub start_block: BlockNumber,
    /// Last SL block to scan, inclusive. `None` means open-ended (tailed against the SL's
    /// finalized boundary).
    pub end_block: Option<BlockNumber>,
}

/// Settlement-layer-aware variant of [`L1Watcher`] that walks a chain of SL segments
/// (L1 → Gateway → L1 → …) in order. Historical segments are scanned to completion once their
/// `start_block`..=`end_block` window is exhausted; if the final segment is open-ended
/// (`end_block = None`) it is tailed live against the finalized boundary so events that haven't
/// yet been irreversibly observed on-chain are not processed. If every segment is closed, the
/// watcher drains them in order and then exits cleanly — useful for scenarios where the active
/// settlement layer no longer emits events of interest (e.g. an interop-root watcher on a chain
/// that has migrated back to L1).
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
        // Only the final segment may be open-ended. Internal open-ended segments are nonsense
        // because they'd never yield to the next one.
        for seg in &segments[..segments.len() - 1] {
            anyhow::ensure!(
                seg.end_block.is_some(),
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
            processor = run_segment(config.clone(), segment, processor).await;
        }
        // Returns once every segment has been fully scanned. For a watcher with an open-ended
        // final segment this is unreachable; for one with only closed segments it terminates
        // cleanly after the historical sweep.
    }
}

async fn run_segment(
    config: L1WatcherConfig,
    segment: SegmentSpec,
    processor: Box<dyn ProcessRawEvents>,
) -> Box<dyn ProcessRawEvents> {
    tracing::info!(
        "sl-aware watcher activated segment at {:?} for SL blocks=({}-{})",
        segment.address,
        segment.start_block,
        segment
            .end_block
            .map(|b| b.to_string())
            .unwrap_or("*".to_string()),
    );

    // Closed segments are bounded by `end_block` (already pre-resolved by the caller against an
    // executed-batch / migration boundary), so the boundary mode does not matter — `end_block`
    // dominates the cap. The open-ended segment uses the finalized boundary so persistence-style
    // processors only react to irreversibly observed events.
    let mut watcher = L1Watcher::new_finalized(
        config,
        segment.provider,
        segment.block_updates,
        segment.address,
        segment.start_block,
        segment.end_block,
        processor,
    );
    watcher.run_inner().await;
    watcher.processor
}
