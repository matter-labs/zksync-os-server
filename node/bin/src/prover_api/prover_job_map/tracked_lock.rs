use crate::prover_api::metrics::{JobMapMethod, PROVER_METRICS, ProverStage};
use std::collections::BTreeMap;
use std::time::Instant;
use tokio::sync::MutexGuard;

use super::models::JobEntry;

/// Mutex that tracks how long a lock is held and reports metrics on drop
pub(super) struct TrackedLockGuard<'a, T> {
    guard: MutexGuard<'a, BTreeMap<u64, JobEntry<T>>>,
    acquired_at: Instant,
    prover_stage: ProverStage,
    method: JobMapMethod,
}

impl<'a, T> TrackedLockGuard<'a, T> {
    pub(super) fn new(
        guard: MutexGuard<'a, BTreeMap<u64, JobEntry<T>>>,
        acquired_at: Instant,
        prover_stage: ProverStage,
        method: JobMapMethod,
    ) -> Self {
        Self {
            guard,
            acquired_at,
            prover_stage,
            method,
        }
    }
}

impl<'a, T> Drop for TrackedLockGuard<'a, T> {
    fn drop(&mut self) {
        let hold_time = self.acquired_at.elapsed();
        PROVER_METRICS.job_map_lock_hold_time[&(self.prover_stage, self.method)].observe(hold_time);
    }
}

impl<'a, T> std::ops::Deref for TrackedLockGuard<'a, T> {
    type Target = MutexGuard<'a, BTreeMap<u64, JobEntry<T>>>;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a, T> std::ops::DerefMut for TrackedLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}
