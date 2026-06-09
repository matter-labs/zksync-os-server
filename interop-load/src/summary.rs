use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub source_submitted: u64,
    pub source_included: u64,
    pub proof_available: u64,
    pub root_imported: u64,
    pub execute_submitted: u64,
    pub execute_included: u64,
    pub failed_classified: u64,
    pub erc20_submitted: u64,
    pub base_submitted: u64,
    pub message_submitted: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub open_loop_violated: bool,
    pub measured_duration_ms: u64,
    pub source_submitted_per_sec: f64,
    pub root_imported_per_sec: f64,
    pub final_backlog: u64,
    pub totals: Totals,
    /// Source→destination interop latency, computed from per-bundle timestamps
    /// of measured bundles that reached `root_imported` on the destination
    /// chain. `None` when no measured bundle completed.
    pub latency: Option<LatencyReport>,
    pub scaffold_only: bool,
}

/// One bundle's stage timestamps (epoch ms), recorded as it propagates. Only
/// bundles that reach `root_imported` contribute a full end-to-end sample.
#[derive(Debug, Clone)]
pub struct LatencySample {
    pub source_chain_id: u64,
    pub destination_chain_id: u64,
    /// `sendBundle` submitted on the source chain.
    pub source_submitted_at_ms: u128,
    /// Source `sendBundle` receipt observed.
    pub source_included_at_ms: u128,
    /// MessageRoot proof became available (gateway-side).
    pub proof_available_at_ms: u128,
    /// Interop root present in the destination chain's root storage.
    pub root_imported_at_ms: u128,
}

/// Latency percentiles in milliseconds for a set of bundles.
#[derive(Debug, Default, Serialize)]
pub struct LatencyStats {
    pub count: u64,
    pub min_ms: u64,
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub max_ms: u64,
    pub mean_ms: u64,
}

/// Per-stage and end-to-end latency breakdown, computed over the same sample
/// set so the stage percentiles add up intuitively (each is the distribution
/// of that stage's own duration, not a decomposition of the p50 total).
#[derive(Debug, Default, Serialize)]
pub struct LatencyBreakdown {
    /// `source_submitted` → `root_imported` (the headline "reach destination" metric).
    pub end_to_end: LatencyStats,
    /// `source_submitted` → `source_included` (source-chain inclusion).
    pub submit_to_included: LatencyStats,
    /// `source_included` → `proof_available` (batch sealing + gateway proof).
    pub included_to_proof: LatencyStats,
    /// `proof_available` → `root_imported` (root import on destination).
    pub proof_to_root: LatencyStats,
}

#[derive(Debug, Serialize)]
pub struct LaneLatency {
    pub source_chain_id: u64,
    pub destination_chain_id: u64,
    #[serde(flatten)]
    pub breakdown: LatencyBreakdown,
}

#[derive(Debug, Serialize)]
pub struct LatencyReport {
    /// Aggregate across all measured, fully-propagated bundles.
    pub aggregate: LatencyBreakdown,
    /// Per source→destination lane, sorted by source then destination chain id.
    pub per_lane: Vec<LaneLatency>,
}

impl LatencyReport {
    /// Builds the report from end-to-end latency samples. Returns `None` if
    /// there are no samples (no measured bundle reached the destination).
    pub fn from_samples(samples: &[LatencySample]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let aggregate = breakdown_from(samples);

        // Group by (source, destination) lane.
        let mut by_lane: BTreeMap<(u64, u64), Vec<&LatencySample>> = BTreeMap::new();
        for sample in samples {
            by_lane
                .entry((sample.source_chain_id, sample.destination_chain_id))
                .or_default()
                .push(sample);
        }
        let per_lane = by_lane
            .into_iter()
            .map(|((source_chain_id, destination_chain_id), lane_samples)| {
                let owned: Vec<LatencySample> = lane_samples.into_iter().cloned().collect();
                LaneLatency {
                    source_chain_id,
                    destination_chain_id,
                    breakdown: breakdown_from(&owned),
                }
            })
            .collect();

        Some(Self {
            aggregate,
            per_lane,
        })
    }
}

fn breakdown_from(samples: &[LatencySample]) -> LatencyBreakdown {
    let end_to_end: Vec<u64> = samples
        .iter()
        .map(|s| saturating_delta(s.root_imported_at_ms, s.source_submitted_at_ms))
        .collect();
    let submit_to_included: Vec<u64> = samples
        .iter()
        .map(|s| saturating_delta(s.source_included_at_ms, s.source_submitted_at_ms))
        .collect();
    let included_to_proof: Vec<u64> = samples
        .iter()
        .map(|s| saturating_delta(s.proof_available_at_ms, s.source_included_at_ms))
        .collect();
    let proof_to_root: Vec<u64> = samples
        .iter()
        .map(|s| saturating_delta(s.root_imported_at_ms, s.proof_available_at_ms))
        .collect();

    LatencyBreakdown {
        end_to_end: LatencyStats::from_durations(end_to_end),
        submit_to_included: LatencyStats::from_durations(submit_to_included),
        included_to_proof: LatencyStats::from_durations(included_to_proof),
        proof_to_root: LatencyStats::from_durations(proof_to_root),
    }
}

/// Clamps to zero if the end timestamp is before the start (clock skew /
/// out-of-order events should never produce a negative latency).
fn saturating_delta(end_ms: u128, start_ms: u128) -> u64 {
    end_ms.saturating_sub(start_ms) as u64
}

impl LatencyStats {
    /// Computes percentiles from a set of per-bundle durations in ms.
    /// `durations` is consumed and sorted in place. Empty input yields all
    /// zeros with `count == 0`.
    pub fn from_durations(mut durations: Vec<u64>) -> Self {
        if durations.is_empty() {
            return Self::default();
        }
        durations.sort_unstable();
        let count = durations.len();
        let sum: u128 = durations.iter().map(|&d| d as u128).sum();
        Self {
            count: count as u64,
            min_ms: durations[0],
            p50_ms: percentile(&durations, 50.0),
            p90_ms: percentile(&durations, 90.0),
            p95_ms: percentile(&durations, 95.0),
            p99_ms: percentile(&durations, 99.0),
            max_ms: durations[count - 1],
            mean_ms: (sum / count as u128) as u64,
        }
    }
}

/// Nearest-rank percentile over a pre-sorted slice. `pct` is in [0, 100].
/// Rank = ceil(pct/100 * n), clamped to [1, n]; index = rank - 1.
fn percentile(sorted: &[u64], pct: f64) -> u64 {
    debug_assert!(!sorted.is_empty());
    let n = sorted.len();
    let rank = ((pct / 100.0) * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 50.0), 50);
        assert_eq!(percentile(&sorted, 90.0), 90);
        assert_eq!(percentile(&sorted, 95.0), 95);
        assert_eq!(percentile(&sorted, 99.0), 99);
        assert_eq!(percentile(&sorted, 100.0), 100);
    }

    #[test]
    fn percentile_single_element() {
        assert_eq!(percentile(&[42], 50.0), 42);
        assert_eq!(percentile(&[42], 99.0), 42);
    }

    #[test]
    fn stats_from_durations_basic() {
        let stats = LatencyStats::from_durations(vec![10, 20, 30, 40, 50]);
        assert_eq!(stats.count, 5);
        assert_eq!(stats.min_ms, 10);
        assert_eq!(stats.max_ms, 50);
        assert_eq!(stats.mean_ms, 30);
        assert_eq!(stats.p50_ms, 30);
    }

    #[test]
    fn empty_durations_yield_zeros() {
        let stats = LatencyStats::from_durations(vec![]);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.p95_ms, 0);
    }

    #[test]
    fn report_splits_by_lane() {
        let samples = vec![
            LatencySample {
                source_chain_id: 6565,
                destination_chain_id: 6566,
                source_submitted_at_ms: 1000,
                source_included_at_ms: 1100,
                proof_available_at_ms: 6000,
                root_imported_at_ms: 9000,
            },
            LatencySample {
                source_chain_id: 6566,
                destination_chain_id: 6567,
                source_submitted_at_ms: 2000,
                source_included_at_ms: 2200,
                proof_available_at_ms: 7000,
                root_imported_at_ms: 12000,
            },
        ];
        let report = LatencyReport::from_samples(&samples).unwrap();
        assert_eq!(report.aggregate.end_to_end.count, 2);
        assert_eq!(report.per_lane.len(), 2);
        // Lanes sorted by (source, destination).
        assert_eq!(report.per_lane[0].source_chain_id, 6565);
        assert_eq!(report.per_lane[0].breakdown.end_to_end.count, 1);
        // 9000 - 1000 = 8000ms end-to-end for lane 0.
        assert_eq!(report.per_lane[0].breakdown.end_to_end.p50_ms, 8000);
        // 6000 - 1100 = 4900ms included→proof for lane 0.
        assert_eq!(report.per_lane[0].breakdown.included_to_proof.p50_ms, 4900);
    }

    #[test]
    fn no_samples_yield_none() {
        assert!(LatencyReport::from_samples(&[]).is_none());
    }

    #[test]
    fn negative_delta_clamps_to_zero() {
        // root_imported before source_submitted (impossible but defend anyway).
        let samples = vec![LatencySample {
            source_chain_id: 1,
            destination_chain_id: 2,
            source_submitted_at_ms: 5000,
            source_included_at_ms: 5100,
            proof_available_at_ms: 6000,
            root_imported_at_ms: 4000,
        }];
        let report = LatencyReport::from_samples(&samples).unwrap();
        assert_eq!(report.aggregate.end_to_end.p50_ms, 0);
    }
}
