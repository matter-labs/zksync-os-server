use itertools::Itertools;
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::time::{Duration, Instant};

/// Helper to track what we spend time on within any continuous/linear, potentially pipelined operation
/// Stages must have a deterministic and linear order.
///
/// Currently only used to track Batch stages (e.g. committing to l1, proving etc):
/// 2025-09-15T16:26:50.305797Z  INFO zksync_os_server::batch_sink: ▶▶▶ Batch has been fully processed
/// batch_number=1 latency_tracker=total: 27.470830125s (ProveL1TxSent: 947.967416ms (3.45%);
/// SnarkProvedFake: 1.015951917s (3.70%); ExecuteL1TxSent: 1.225118875s (4.46%);
/// ExecuteL1TxMined: 10.730509125s (39.06%); ProveL1TxMined: 13.551282792s (49.33%); )
/// tx_count=7 block_from=1 block_to=2 proof=Fake
///
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

impl<S: Debug> Display for LatencyDistributionTracker<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let total = self.past_stages.iter().map(|(_, d)| d).sum::<Duration>();
        write!(f, "total: {total:?} (")?;
        self.past_stages
            .iter()
            .sorted_by_key(|(_, v)| *v)
            .for_each(|(stage, duration)| {
                let percentage = duration.div_duration_f32(total) * 100f32;
                write!(f, "{stage:?}: {duration:?} ({percentage:.2}%); ").unwrap()
            });
        write!(f, ")")?;
        Ok(())
    }
}
