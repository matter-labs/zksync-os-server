use crate::watcher::L1Watcher;
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

/// Boxed async closure that turns a starting point `S` into the full segment list and the
/// processor `P` that consumes it.
type ResolveSegmentsFn<S, P> =
    Box<dyn FnOnce(S) -> BoxFuture<'static, anyhow::Result<(Vec<SegmentSpec>, P)>> + Send + Sync>;

/// Deferred constructor for an [`SlAwareL1Watcher`]: turns a starting point `S` into a
/// ready-to-run watcher once that starting point is finally known.
///
/// Mirrors [`StartResolver`](crate::watcher::StartResolver) but yields the full segment list
/// (each segment's `start_block`/`end_block` resolved via per-segment binary searches) instead
/// of a single block.
pub struct SegmentResolver<S, P> {
    config: L1WatcherConfig,
    resolve_segments: ResolveSegmentsFn<S, P>,
}

impl<S, P: ProcessRawEvents> SegmentResolver<S, P> {
    pub(crate) fn new<Fut>(
        config: L1WatcherConfig,
        resolve_segments: impl FnOnce(S) -> Fut + Send + Sync + 'static,
    ) -> Self
    where
        Fut: Future<Output = anyhow::Result<(Vec<SegmentSpec>, P)>> + Send + 'static,
    {
        Self {
            config,
            resolve_segments: Box::new(move |start| Box::pin(resolve_segments(start))),
        }
    }

    /// Resolves the starting point into a segment list and processor, producing a ready-to-run
    /// [`SlAwareL1Watcher`].
    pub async fn resolve(self, start: S) -> anyhow::Result<SlAwareL1Watcher<P>> {
        let (segments, processor) = (self.resolve_segments)(start).await?;
        SlAwareL1Watcher::new(self.config, segments, processor)
    }

    /// Resolves the starting point and runs the produced watcher. A failure to resolve the
    /// segments is fatal (panics), matching the previous behavior where resolution happened at
    /// construction.
    pub async fn run(self, start: S) {
        self.resolve(start)
            .await
            .expect("failed to resolve SL-aware watcher segments")
            .run()
            .await;
    }
}

/// Settlement-layer-aware variant of [`L1Watcher`] that walks a chain of SL segments
/// (L1 → Gateway → L1 → …) in order. Historical segments are scanned to completion once their
/// `start_block`..=`end_block` window is exhausted; if the final segment is open-ended
/// (`end_block = None`) it is tailed live against the finalized boundary so events that haven't
/// yet been irreversibly observed on-chain are not processed. If every segment is closed, the
/// watcher drains them in order and then exits cleanly — useful for scenarios where the active
/// settlement layer no longer emits events of interest (e.g. an interop-root watcher on a chain
/// that has migrated back to L1).
pub struct SlAwareL1Watcher<P> {
    config: L1WatcherConfig,
    segments: VecDeque<SegmentSpec>,
    processor: P,
}

impl<P: ProcessRawEvents> SlAwareL1Watcher<P> {
    pub(crate) fn new(
        config: L1WatcherConfig,
        segments: Vec<SegmentSpec>,
        processor: P,
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

async fn run_segment<P: ProcessRawEvents>(
    config: L1WatcherConfig,
    segment: SegmentSpec,
    processor: P,
) -> P {
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
        segment.address,
        segment.start_block,
        segment.end_block,
        processor,
    );
    watcher.run_inner().await;
    watcher.processor
}
