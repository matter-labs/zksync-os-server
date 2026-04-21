use crate::config::ComponentId;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::watch;
use zksync_os_observability::ComponentState;

/// Pre-computed coordinate maps extracted from component state receivers.
///
/// Both the backpressure monitor and the `/status/pipeline` HTTP handler need
/// identical snapshot logic (processed/picked fallbacks, batch fallbacks). Extracting
/// this into a shared struct ensures the two code paths cannot diverge.
pub struct PipelineMaps {
    pub processed: HashMap<ComponentId, (u64, Option<u64>)>,
    pub picked: HashMap<ComponentId, (u64, Option<u64>)>,
    pub batch_processed: HashMap<ComponentId, u64>,
    pub batch_picked: HashMap<ComponentId, u64>,
}

impl PipelineMaps {
    /// Snapshot the current coordinates from all registered component state receivers.
    ///
    /// Fallback policy — block and batch coordinates are symmetric:
    /// - `picked` falls back to `last_processed` when `last_picked` is `None`.
    ///   This is intentional for components like L1Sender / UpgradeGatekeeper that drain
    ///   the channel before slow async work; adjacent lag measures processing progress,
    ///   not channel occupancy.
    /// - When both `last_picked` and `last_processed` are `None`, `picked` is recorded
    ///   as `(0, None)`. Adjacent lag must still fire for a component stuck before its
    ///   first pick/process (e.g. L1Sender waiting for its first L1 receipt) — otherwise
    ///   a cold-start stall can never trip its own threshold.
    /// - `batch_picked` falls back to `batch_number` when `last_batch_picked` is `None`,
    ///   for exactly the same reason as the block-level fallback. Without this,
    ///   `max_batch_diff_to_upstream` thresholds configured for components that deliberately skip
    ///   an explicit `last_batch_picked` (L1Sender*, UpgradeGatekeeper never call
    ///   `record_picked` at all) would be silently disabled despite being present in
    ///   the config.
    pub fn snapshot(components: &[(ComponentId, watch::Receiver<ComponentState>)]) -> Self {
        let states: Vec<(ComponentId, ComponentState)> = components
            .iter()
            .map(|(id, rx)| (*id, rx.borrow().clone()))
            .collect();
        Self::snapshot_from(&states)
    }

    /// Same fallback policy as [`Self::snapshot`], but built from states the caller
    /// has already observed — for callers that must expose additional fields from the
    /// same observation and cannot afford a mid-computation re-borrow.
    pub fn snapshot_from(states: &[(ComponentId, ComponentState)]) -> Self {
        let mut processed = HashMap::new();
        let mut picked = HashMap::new();
        let mut batch_processed = HashMap::new();
        let mut batch_picked = HashMap::new();

        for (id, h) in states {
            if let Some(c) = h.last_processed.as_ref() {
                processed.insert(*id, (c.block_number, c.timestamp));
            }
            let picked_coord = h
                .last_picked
                .as_ref()
                .or(h.last_processed.as_ref())
                .map(|c| (c.block_number, c.timestamp))
                .unwrap_or((0, None));
            picked.insert(*id, picked_coord);

            if let Some(batch_num) = h.batch_number {
                batch_processed.insert(*id, batch_num);
            }
            if let Some(bp) = h.last_batch_picked.or(h.batch_number) {
                batch_picked.insert(*id, bp);
            }
        }

        Self {
            processed,
            picked,
            batch_processed,
            batch_picked,
        }
    }
}

pub struct AdjacentSnapshot {
    /// upstream_block − downstream_block (saturating)
    pub block_diff: u64,
    /// upstream_ts − downstream_ts as Duration; None when either timestamp is absent.
    pub time_diff: Option<Duration>,
    /// upstream.batch_number − downstream.(last_batch_picked ?? batch_number) (saturating).
    /// None when either component has no batch tracking at all (block-pipeline components
    /// that never set `batch_number`). See `PipelineMaps::snapshot` for the
    /// fallback policy; it mirrors the block-level `picked → last_processed` fallback.
    pub batch_diff: Option<u64>,
}

/// Compute adjacent block and time diffs for each downstream component.
///
/// `edges` is an explicit list of (upstream, downstream) pairs, which allows fan-in/fan-out
/// topologies impossible with a linear slice. Each edge produces at most one entry in the
/// returned map (keyed by downstream ComponentId). Components for which either processed or
/// picked maps lack an entry are silently skipped.
///
/// Block diff = upstream.last_processed − downstream.last_picked (channel occupancy).
pub fn compute_adjacent_snapshots(
    edges: &[(ComponentId, ComponentId)],
    processed: &HashMap<ComponentId, (u64, Option<u64>)>,
    picked: &HashMap<ComponentId, (u64, Option<u64>)>,
    batch_processed: &HashMap<ComponentId, u64>,
    batch_picked: &HashMap<ComponentId, u64>,
) -> HashMap<ComponentId, AdjacentSnapshot> {
    edges
        .iter()
        .filter_map(|&(up, down)| {
            let &(upstream_block, upstream_ts) = processed.get(&up)?;
            let &(downstream_block, downstream_ts) = picked.get(&down)?;
            let block_diff = upstream_block.saturating_sub(downstream_block);
            let time_diff = match (upstream_ts, downstream_ts) {
                (Some(u), Some(d)) => Some(Duration::from_secs(u.saturating_sub(d))),
                _ => None,
            };
            let batch_diff = batch_processed
                .get(&up)
                .zip(batch_picked.get(&down))
                .map(|(&u, &d)| u.saturating_sub(d));
            Some((
                down,
                AdjacentSnapshot {
                    block_diff,
                    time_diff,
                    batch_diff,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(block: u64, ts: Option<u64>) -> (u64, Option<u64>) {
        (block, ts)
    }

    #[test]
    fn block_diff_is_upstream_minus_downstream() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(100, None));
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, None));
        let edges = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &edges,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(result[&ComponentId::BlockCanonizer].block_diff, 10);
        assert!(result[&ComponentId::BlockCanonizer].time_diff.is_none());
    }

    #[test]
    fn time_diff_is_upstream_minus_downstream_duration() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(100, Some(2000)));
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, Some(1960)));
        let edges = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &edges,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(
            result[&ComponentId::BlockCanonizer].time_diff,
            Some(Duration::from_secs(40))
        );
    }

    #[test]
    fn time_diff_is_none_when_upstream_timestamp_absent() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(100, None));
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, Some(1960)));
        let edges = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &edges,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(result[&ComponentId::BlockCanonizer].time_diff.is_none());
    }

    #[test]
    fn time_diff_is_none_when_downstream_timestamp_absent() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(100, Some(2000)));
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, None));
        let edges = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &edges,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(result[&ComponentId::BlockCanonizer].time_diff.is_none());
    }

    #[test]
    fn block_diff_saturates_when_downstream_ahead() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(90, None));
        snapshots.insert(ComponentId::BlockCanonizer, snap(100, None));
        let edges = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &edges,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(result[&ComponentId::BlockCanonizer].block_diff, 0);
    }

    #[test]
    fn multi_hop_chain_produces_per_hop_diffs() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(200, Some(2000)));
        snapshots.insert(ComponentId::BlockCanonizer, snap(195, Some(1950)));
        snapshots.insert(ComponentId::BlockApplier, snap(193, Some(1930)));
        let edges = vec![
            (ComponentId::BlockExecutor, ComponentId::BlockCanonizer),
            (ComponentId::BlockCanonizer, ComponentId::BlockApplier),
        ];
        let result = compute_adjacent_snapshots(
            &edges,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(result[&ComponentId::BlockCanonizer].block_diff, 5);
        assert_eq!(
            result[&ComponentId::BlockCanonizer].time_diff,
            Some(Duration::from_secs(50))
        );
        assert_eq!(result[&ComponentId::BlockApplier].block_diff, 2);
        assert_eq!(
            result[&ComponentId::BlockApplier].time_diff,
            Some(Duration::from_secs(20))
        );
    }

    #[test]
    fn empty_edges_returns_empty_map() {
        let snapshots: HashMap<ComponentId, (u64, Option<u64>)> = HashMap::new();
        let result = compute_adjacent_snapshots(
            &[],
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn missing_upstream_skips_pair() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, None));
        let edges = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &edges,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn block_diff_uses_upstream_processed_minus_downstream_picked() {
        let mut processed = HashMap::new();
        processed.insert(ComponentId::BlockExecutor, (100u64, None));

        let mut picked = HashMap::new();
        picked.insert(ComponentId::BlockCanonizer, (95u64, None));

        let edges = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &edges,
            &processed,
            &picked,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(result[&ComponentId::BlockCanonizer].block_diff, 5);
    }

    /// Edges drive topology even when map ordering disagrees.
    /// A component registered in one order can still be wired to any upstream via explicit edges.
    #[test]
    fn edges_drive_topology_independent_of_map_insertion_order() {
        let mut processed = HashMap::new();
        processed.insert(ComponentId::BlockCanonizer, (90u64, None));
        processed.insert(ComponentId::BlockExecutor, (100u64, None));
        processed.insert(ComponentId::BlockApplier, (80u64, None));

        let mut picked = HashMap::new();
        picked.insert(ComponentId::BlockCanonizer, (90u64, None));
        picked.insert(ComponentId::BlockApplier, (80u64, None));

        let edges = vec![
            (ComponentId::BlockExecutor, ComponentId::BlockApplier),
            (ComponentId::BlockApplier, ComponentId::BlockCanonizer),
        ];

        let result = compute_adjacent_snapshots(
            &edges,
            &processed,
            &picked,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(result[&ComponentId::BlockApplier].block_diff, 20);
        assert_eq!(result[&ComponentId::BlockCanonizer].block_diff, 0);
    }

    /// The block-level `picked → last_processed` fallback has a batch-level twin:
    /// `batch_picked → batch_number`. A component that never calls `record_picked`
    /// (L1Sender, UpgradeGatekeeper) must still produce a non-None `batch_diff` so its
    /// `max_batch_diff_to_upstream` threshold is not silently disabled.
    #[tokio::test]
    async fn batch_picked_falls_back_to_batch_number() {
        use zksync_os_observability::ComponentStateReporter;
        let (up_reporter, up_rx) = ComponentStateReporter::new("upstream");
        let (down_reporter, down_rx) = ComponentStateReporter::new("downstream");
        up_reporter.record_processed(100, None, Some(10));
        down_reporter.record_processed(95, None, Some(8));
        // downstream deliberately does NOT call record_picked — L1Sender-like.

        let components = vec![
            (ComponentId::UpgradeGatekeeper, up_rx),
            (ComponentId::L1SenderCommit, down_rx),
        ];
        let maps = PipelineMaps::snapshot(&components);
        let edges = vec![(ComponentId::UpgradeGatekeeper, ComponentId::L1SenderCommit)];
        let result = compute_adjacent_snapshots(
            &edges,
            &maps.processed,
            &maps.picked,
            &maps.batch_processed,
            &maps.batch_picked,
        );

        let snap = &result[&ComponentId::L1SenderCommit];
        assert_eq!(
            snap.batch_diff,
            Some(2),
            "fallback must produce batch_diff = upstream.batch_number − downstream.batch_number"
        );
    }

    /// Explicit `last_batch_picked` takes precedence over the `batch_number` fallback.
    #[tokio::test]
    async fn explicit_batch_picked_wins_over_batch_number_fallback() {
        use zksync_os_observability::ComponentStateReporter;
        let (up_reporter, up_rx) = ComponentStateReporter::new("upstream");
        let (down_reporter, down_rx) = ComponentStateReporter::new("downstream");
        up_reporter.record_processed(100, None, Some(10));
        down_reporter.record_processed(90, None, Some(5));
        down_reporter.record_picked(95, None, Some(7));

        let components = vec![
            (ComponentId::BatchVerification, up_rx),
            (ComponentId::FriJobManager, down_rx),
        ];
        let maps = PipelineMaps::snapshot(&components);
        let edges = vec![(ComponentId::BatchVerification, ComponentId::FriJobManager)];
        let result = compute_adjacent_snapshots(
            &edges,
            &maps.processed,
            &maps.picked,
            &maps.batch_processed,
            &maps.batch_picked,
        );

        assert_eq!(
            result[&ComponentId::FriJobManager].batch_diff,
            Some(3),
            "explicit last_batch_picked=7 must be used, not batch_number=5 fallback"
        );
    }
}
