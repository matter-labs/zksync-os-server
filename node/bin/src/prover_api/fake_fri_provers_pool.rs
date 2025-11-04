use futures::future::try_join_all;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep};

use crate::prover_api::fri_job_manager::FriJobManager;

const POLL_INTERVAL_MS: u64 = 250;
const PROVER_LABEL: &str = "fake_prover";

/// Emulates a pool of provers:
/// - Picks jobs whose inbound age is at least `min_inbound_age`,
/// - Waits `compute_time` to emulate proving,
/// - Submits a fake proof via `ProverJobManager::submit_fake_proof`.
#[derive(Clone, Debug)]
pub struct FakeFriProversPool {
    job_manager: Arc<FriJobManager>,
    workers: usize,
    compute_time: Duration,
    min_inbound_age: Duration,
}

impl FakeFriProversPool {
    pub fn new(
        job_manager: Arc<FriJobManager>,
        workers: usize,
        compute_time: Duration,
        min_inbound_age: Duration,
    ) -> Self {
        Self {
            job_manager,
            workers,
            compute_time,
            min_inbound_age,
        }
    }

    /// Run the fake prover pool. Spawns `workers` tasks and waits for them.
    pub async fn run(self) -> anyhow::Result<()> {
        let mut joins = Vec::with_capacity(self.workers);
        for _ in 0..self.workers {
            let jm = Arc::clone(&self.job_manager);
            let compute_time = self.compute_time;
            let min_age = self.min_inbound_age;

            let handle = tokio::spawn(async move {
                loop {
                    // Only take inbound items whose age >= min_age.
                    match jm.pick_next_job(min_age) {
                        Some((fri_job, _prover_input)) => {
                            // Emulate proving work.
                            let start = Instant::now();
                            sleep(compute_time).await;

                            if let Err(e) = jm
                                .submit_fake_proof(fri_job.batch_number, PROVER_LABEL)
                                .await
                            {
                                tracing::warn!(
                                    fri_job.batch_number,
                                    ?e,
                                    elapsed_ms = start.elapsed().as_millis() as u64,
                                    "fake prover failed to submit proof"
                                );
                            } else {
                                tracing::info!(
                                    fri_job.batch_number,
                                    elapsed_ms = start.elapsed().as_millis() as u64,
                                    "fake prover submitted proof"
                                );
                            }
                        }
                        None => {
                            // Nothing eligible now; back off a bit.
                            sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
                        }
                    }
                }
            });
            joins.push(handle);
        }

        try_join_all(joins).await?;
        Ok(())
    }
}
