use crate::config::ComponentId;
use std::collections::HashMap;
use std::time::Duration;

pub struct AdjacentSnapshot {
    /// upstream_seq − downstream_seq (saturating)
    pub block_diff: u64,
    /// upstream_ts − downstream_ts as Duration; None when either timestamp is absent.
    pub time_diff: Option<Duration>,
    /// upstream.batch_number − downstream.last_batch_picked (saturating).
    /// None when either component has no batch tracking (block-pipeline components).
    pub batch_diff: Option<u64>,
}

/// Compute adjacent block and time diffs for each downstream component.
///
/// `adjacency` is a slice of (upstream, downstream) pairs.
/// `processed` maps each ComponentId to its last_processed (block_number, timestamp).
/// `picked` maps each ComponentId to its last_picked (block_number, timestamp).
///
/// Block diff = upstream.last_processed − downstream.last_picked
/// This gives pure channel occupancy: blocks forwarded by upstream but not yet
/// dequeued by downstream.
///
/// Returns a HashMap keyed by downstream ComponentId. Components with no upstream adjacency
/// pair are absent from the result — callers treat their lag as 0 (BlockExecutor, the head)
/// or ignore the result (unmonitored components with no thresholds).
/// The monitor's startup assert guarantees that every other monitored component has a pair.
/// Adjacency pairs where either component is absent from the respective map are silently
/// skipped, so callers in HTTP handlers or other contexts where a panic is unsafe get a
/// graceful result.
///
/// # Panics
/// - If a downstream component appears in more than one pair (fan-in topology).
pub fn compute_adjacent_snapshots(
    adjacency: &[(ComponentId, ComponentId)],
    processed: &HashMap<ComponentId, (u64, Option<u64>)>,
    picked: &HashMap<ComponentId, (u64, Option<u64>)>,
    batch_processed: &HashMap<ComponentId, u64>,
    batch_picked: &HashMap<ComponentId, u64>,
) -> HashMap<ComponentId, AdjacentSnapshot> {
    // Fan-in guard: HashMap::insert would silently overwrite earlier entries.
    {
        let mut seen = std::collections::HashSet::new();
        for &(_, down) in adjacency {
            assert!(
                seen.insert(down),
                "fan-in topology detected: downstream component {:?} appears in multiple \
                 adjacency pairs; compute_adjacent_snapshots does not support this",
                down
            );
        }
    }

    adjacency
        .iter()
        .filter_map(|&(up, down)| {
            let &(up_seq, up_ts) = processed.get(&up)?;
            let &(down_seq, down_ts) = picked.get(&down)?;
            let block_diff = up_seq.saturating_sub(down_seq);
            let time_diff = match (up_ts, down_ts) {
                (Some(u), Some(d)) => Some(Duration::from_secs(u.saturating_sub(d))),
                _ => None,
            };
            let batch_diff = batch_processed
                .get(&up)
                .zip(batch_picked.get(&down))
                .map(|(&up_batch, &down_batch)| up_batch.saturating_sub(down_batch));
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

    fn snap(seq: u64, ts: Option<u64>) -> (u64, Option<u64>) {
        (seq, ts)
    }

    #[test]
    fn block_diff_is_upstream_minus_downstream() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(100, None));
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, None));
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &adjacency,
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
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &adjacency,
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
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &adjacency,
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
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &adjacency,
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
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &adjacency,
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
        let adjacency = vec![
            (ComponentId::BlockExecutor, ComponentId::BlockCanonizer),
            (ComponentId::BlockCanonizer, ComponentId::BlockApplier),
        ];
        let result = compute_adjacent_snapshots(
            &adjacency,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
        // Canonizer diff from Executor: 200-195=5 blocks, 2000-1950=50s
        assert_eq!(result[&ComponentId::BlockCanonizer].block_diff, 5);
        assert_eq!(
            result[&ComponentId::BlockCanonizer].time_diff,
            Some(Duration::from_secs(50))
        );
        // Applier diff from Canonizer: 195-193=2 blocks, 1950-1930=20s
        assert_eq!(result[&ComponentId::BlockApplier].block_diff, 2);
        assert_eq!(
            result[&ComponentId::BlockApplier].time_diff,
            Some(Duration::from_secs(20))
        );
    }

    #[test]
    fn empty_adjacency_returns_empty_map() {
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
    #[should_panic(expected = "fan-in topology detected")]
    fn fan_in_panics() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(100, None));
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, None));
        snapshots.insert(ComponentId::BlockApplier, snap(80, None));
        // Two upstreams → same downstream: fan-in
        let adjacency = vec![
            (ComponentId::BlockExecutor, ComponentId::BlockApplier),
            (ComponentId::BlockCanonizer, ComponentId::BlockApplier),
        ];
        compute_adjacent_snapshots(
            &adjacency,
            &snapshots,
            &snapshots,
            &HashMap::new(),
            &HashMap::new(),
        );
    }

    #[test]
    fn missing_upstream_skips_pair() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, None));
        // BlockExecutor absent from processed map — pair is silently skipped, no panic
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &adjacency,
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
        // downstream picked at 95 (5 blocks sitting in the channel)
        picked.insert(ComponentId::BlockCanonizer, (95u64, None));

        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(
            &adjacency,
            &processed,
            &picked,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(result[&ComponentId::BlockCanonizer].block_diff, 5);
    }
}
