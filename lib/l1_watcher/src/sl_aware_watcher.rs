use crate::watcher::RunningL1Watcher;
use crate::{L1WatcherConfig, ProcessRawEvents};
use alloy::primitives::{Address, BlockNumber};
use alloy::rpc::types::ValueOrArray;
use futures::future::BoxFuture;
use std::collections::VecDeque;
use zksync_os_provider::NodeProvider;

/// Description of a single settlement-layer segment that [`SlAwareL1Watcher`] should scan, in
/// isolation, before advancing to the next one. `end_block = None` marks the open-ended (live)
/// segment; it must appear at most once, as the final entry.
///
/// Block boundaries are pre-resolved by the caller.
#[derive(Clone, Debug)]
pub struct SegmentSpec {
    /// Provider for the settlement layer this segment is scanned on.
    pub provider: NodeProvider,
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
pub struct SlAwareL1Watcher<S> {
    config: L1WatcherConfig,
    resolve_segments: SegmentResolver<S>,
}

/// Resolves an SL-aware watcher's segments and processor once the starting point `S` is known.
///
/// Mirrors [`StartResolver`](crate::watcher::StartResolver) but yields the full segment list
/// (each segment's `start_block`/`end_block` resolved via per-segment binary searches) instead
/// of a single block.
pub(crate) type SegmentResolver<S> = Box<
    dyn FnOnce(
            S,
        )
            -> BoxFuture<'static, anyhow::Result<(Vec<SegmentSpec>, Box<dyn ProcessRawEvents>)>>
        + Send,
>;

/// Builds a [`SegmentResolver`] from an async closure, hiding the `Box::new`/`Box::pin` ceremony.
pub(crate) fn segment_resolver<S, Fut>(
    f: impl FnOnce(S) -> Fut + Send + 'static,
) -> SegmentResolver<S>
where
    Fut: std::future::Future<Output = anyhow::Result<(Vec<SegmentSpec>, Box<dyn ProcessRawEvents>)>>
        + Send
        + 'static,
{
    Box::new(move |s| Box::pin(f(s)))
}

impl<S> SlAwareL1Watcher<S> {
    pub fn new(config: L1WatcherConfig, resolve_segments: SegmentResolver<S>) -> Self {
        Self {
            config,
            resolve_segments,
        }
    }

    pub async fn run(self, start: S)
    where
        S: Send + 'static,
    {
        let Self {
            config,
            resolve_segments,
        } = self;
        let (segments, mut processor) = resolve_segments(start)
            .await
            .expect("failed to resolve SL-aware watcher segments");
        let mut segments = validate_segments(segments).expect("invalid SL-aware watcher segments");
        while let Some(segment) = segments.pop_front() {
            processor = run_segment(config.clone(), segment, processor).await;
        }
        // Returns once every segment has been fully scanned. For a watcher with an open-ended
        // final segment this is unreachable; for one with only closed segments it terminates
        // cleanly after the historical sweep.
    }
}

fn validate_segments(segments: Vec<SegmentSpec>) -> anyhow::Result<VecDeque<SegmentSpec>> {
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
    Ok(segments.into())
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
    let mut watcher = RunningL1Watcher::new_finalized(
        config,
        segment.provider,
        segment.address,
        segment.start_block,
        segment.end_block,
        processor,
    );
    watcher.run_inner().await;
    watcher.processor
}
