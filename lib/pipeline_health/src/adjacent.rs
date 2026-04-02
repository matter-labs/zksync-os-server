use crate::config::ComponentId;
use std::collections::HashMap;
use std::time::Duration;

pub struct AdjacentSnapshot {
    /// upstream_seq − downstream_seq (saturating)
    pub block_diff: u64,
    /// upstream_ts − downstream_ts as Duration; None when either timestamp is absent.
    pub time_diff: Option<Duration>,
}

/// Compute adjacent block and time diffs for each downstream component.
///
/// `adjacency` is a slice of (upstream, downstream) pairs.
/// `snapshots` maps each ComponentId to (last_processed_block_seq, last_processed_block_timestamp).
///
/// Returns a HashMap keyed by downstream ComponentId. Components with no upstream adjacency
/// pair are absent from the result — callers treat their lag as 0 (BlockExecutor, the head)
/// or ignore the result (unmonitored components with no thresholds).
/// The monitor's startup assert guarantees that every other monitored component has a pair.
/// Adjacency pairs where either component is absent from `snapshots` are silently skipped,
/// so callers in HTTP handlers or other contexts where a panic is unsafe get a graceful result.
///
/// # Panics
/// - If a downstream component appears in more than one pair (fan-in topology).
pub fn compute_adjacent_snapshots(
    adjacency: &[(ComponentId, ComponentId)],
    snapshots: &HashMap<ComponentId, (u64, Option<u64>)>,
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
            let &(up_seq, up_ts) = snapshots.get(&up)?;
            let &(down_seq, down_ts) = snapshots.get(&down)?;
            let block_diff = up_seq.saturating_sub(down_seq);
            let time_diff = match (up_ts, down_ts) {
                (Some(u), Some(d)) => Some(Duration::from_secs(u.saturating_sub(d))),
                _ => None,
            };
            Some((
                down,
                AdjacentSnapshot {
                    block_diff,
                    time_diff,
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
        let result = compute_adjacent_snapshots(&adjacency, &snapshots);
        assert_eq!(result[&ComponentId::BlockCanonizer].block_diff, 10);
        assert!(result[&ComponentId::BlockCanonizer].time_diff.is_none());
    }

    #[test]
    fn time_diff_is_upstream_minus_downstream_duration() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(100, Some(2000)));
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, Some(1960)));
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(&adjacency, &snapshots);
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
        let result = compute_adjacent_snapshots(&adjacency, &snapshots);
        assert!(result[&ComponentId::BlockCanonizer].time_diff.is_none());
    }

    #[test]
    fn time_diff_is_none_when_downstream_timestamp_absent() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(100, Some(2000)));
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, None));
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(&adjacency, &snapshots);
        assert!(result[&ComponentId::BlockCanonizer].time_diff.is_none());
    }

    #[test]
    fn block_diff_saturates_when_downstream_ahead() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockExecutor, snap(90, None));
        snapshots.insert(ComponentId::BlockCanonizer, snap(100, None));
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(&adjacency, &snapshots);
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
        let result = compute_adjacent_snapshots(&adjacency, &snapshots);
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
        let result = compute_adjacent_snapshots(&[], &snapshots);
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
        compute_adjacent_snapshots(&adjacency, &snapshots);
    }

    #[test]
    fn missing_upstream_skips_pair() {
        let mut snapshots = HashMap::new();
        snapshots.insert(ComponentId::BlockCanonizer, snap(90, None));
        // BlockExecutor absent from snapshots — pair is silently skipped, no panic
        let adjacency = vec![(ComponentId::BlockExecutor, ComponentId::BlockCanonizer)];
        let result = compute_adjacent_snapshots(&adjacency, &snapshots);
        assert!(result.is_empty());
    }
}
