use itertools::Itertools;
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::time::{Duration, Instant};

/// Helper to track what we spend time on within any continuous/linear, potentially pipelined operation
/// Stages must have a deterministic and linear order.
/// Currently only used to track Batch stages (e.g. committing to l1, proving etc)
/// Can potentially be used for Block stages (eg with consensus), but:
/// * currently there is no pipelining within block production, so there is little value
/// * block stages are not linear - we alternate between WaitingForTxs and VmExecuting.

#[derive(Debug)]
pub struct LatencyDistributionTracker<S> {
    last_stage_started_at: Instant,
    past_stages: Vec<(S, Duration)>,
}

impl<S> Default for LatencyDistributionTracker<S> {
    fn default() -> Self {
        Self {
            last_stage_started_at: Instant::now(),
            past_stages: vec![],
        }
    }
}

impl<S> LatencyDistributionTracker<S> {
    pub fn record_stage<F>(&mut self, stage: S, track_latency: F)
    where
        F: FnOnce(Duration),
    {
        let duration = self.last_stage_started_at.elapsed();
        self.last_stage_started_at = Instant::now();
        self.past_stages.push((stage, duration));
        track_latency(duration);
    }

    pub fn current_stage_age(&self) -> Duration {
        self.last_stage_started_at.elapsed()
    }
}

impl<S: Debug + Copy + Eq + std::hash::Hash> Display for LatencyDistributionTracker<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let total = self.past_stages.iter().map(|(_, d)| d).sum::<Duration>();
        write!(f, "total: {total:?} (")?;
        self.past_stages
            .iter()
            .sorted_by_key(|(_, v)| *v)
            .for_each(|(stage, duration)| {
                let percentage = duration.div_duration_f32(total) * 100f32;
                write!(f, "{stage:?}: {duration:?} ({percentage}%); ").unwrap()
            });
        write!(f, ")")?;
        Ok(())
    }
}
