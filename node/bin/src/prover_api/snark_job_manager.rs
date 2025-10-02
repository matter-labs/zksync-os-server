use crate::util::peekable_receiver::PeekableReceiver;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope, SnarkProof};
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_observability::{
    ComponentStateHandle, ComponentStateReporter, GenericComponentState,
};

/// Job manager for SNARK proving.
///
/// Doesn't support multiple provers yet (they'd get the same job)
///
/// Supports both real and fake proofs.
///  - Fake FRI proofs always result in fake SNARK proofs.
///  - Real FRI proofs may result in real or fake SNARK proofs depending on prover availability
///
/// `SnarkJobManager` aims to assign real prover jobs to real SNARK provers -
///     but if jobs are not picked within a timeout (`max_batch_age`), it falls back to a fake proof.
///
/// Jobs from the inbound channel are picked if one of the following apply:
///  - (1) Oldest Pending batch (head of `committed_batch_receiver`) is older than `max_batch_age`
///     - we then consume all batches from the channel until:
///          - we stumble upon a real FRI proof not timed out yet OR
///          - after `max_fris_per_snark` batches.
///  - (2) Real SNARK prover requests a job
///     - we then first consume all fake FRI proofs (turning them into a fake `SNARK`)
///     - afterwards, we consume real FRI proofs from the channel until:
///         - we stumble upon a fake FRI proof OR
///         - after `max_fris_per_snark` batches.
///
///
/// This way we provide the following guarantees (in this order):
///     * no jobs older than `max_batch_age` stay in the queue
///     * real FRI proofs are not discarded (by faking SNARKs)
///     * fake SNARKs aim include maximum number of FRIs possible
///
/// `ComponentStateLatencyTracker`: Only tracks `Processing` / `WaitingSend` states
pub struct SnarkJobManager {
    // == plumbing ==
    // inbound
    committed_batch_receiver: Mutex<PeekableReceiver<SignedBatchEnvelope<FriProof>>>,
    // outbound
    prove_batches_sender: Sender<ProofCommand>,

    // config
    max_fris_per_snark: usize,
    // metrics
    latency_tracker: ComponentStateHandle<GenericComponentState>,
}

impl SnarkJobManager {
    pub fn new(
        // == plumbing ==
        // inbound
        committed_batch_receiver: PeekableReceiver<SignedBatchEnvelope<FriProof>>,
        // outbound
        prove_batches_sender: Sender<ProofCommand>,
        // config
        max_fris_per_snark: usize,
    ) -> Self {
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "snark_job_manager",
            GenericComponentState::ProcessingOrWaitingRecv,
        );
        let committed_batch_receiver = Mutex::new(committed_batch_receiver);
        Self {
            committed_batch_receiver,
            prove_batches_sender,
            max_fris_per_snark,
            latency_tracker,
        }
    }

    // If there is a job pending, returns a non-empty list of tuples (`batch_number`, `real_fri_proof`)
    pub async fn pick_real_job(&self) -> anyhow::Result<Option<Vec<(u64, FriProof)>>> {
        self.consume_fake_proves_from_head(None).await?;
        // note that here we don't consume the messages from channel -
        // the job will be picked, but there is no guarantee it will be completed
        let batches_with_real_proofs: Vec<(u64, FriProof)> = self
            .committed_batch_receiver
            .lock()
            .await
            .peek_until(self.max_fris_per_snark, |envelope| {
                if envelope.data.is_fake() {
                    None
                } else {
                    Some((envelope.batch_number(), envelope.data.clone()))
                }
            });
        if batches_with_real_proofs.is_empty() {
            return Ok(None);
        }
        tracing::info!(
            "real SNARK proof for batches {}-{} is picked by a prover",
            batches_with_real_proofs.first().unwrap().0,
            batches_with_real_proofs.last().unwrap().0,
        );
        Ok(Some(batches_with_real_proofs))
    }

    pub async fn submit_proof(
        &self,
        batch_from: u64,
        batch_to: u64,
        payload: Vec<u8>,
    ) -> anyhow::Result<()> {
        let mut receiver = self.committed_batch_receiver.lock().await;

        // first check that queue is consistent with the submitted proof
        //    (so that pending batch didn't time out or got proven by other prover)
        // we check the HEAD batch number equals to `batch_from`
        let pending_batch_number = receiver.peek_with(|envelope| envelope.batch_number());
        match pending_batch_number {
            Some(expected_batch_number) if batch_from != expected_batch_number => {
                anyhow::bail!(
                    "Batch range error. Expected first batch: {}, received: {}-{}",
                    expected_batch_number,
                    batch_from,
                    batch_to
                );
            }
            None => {
                anyhow::bail!("No pending batches to prove")
            }
            _ => {
                tracing::debug!(
                    "submitted proof is consistent with queue state. (proof for batches {}-{})",
                    batch_from,
                    batch_to
                );
            }
        }

        let batches_proven = receiver
            // we don't apply max_fris_per_snark when accepting complete jobs (maybe it was changed after job was picked)
            .peek_until(usize::MAX, |envelope| {
                if envelope.batch_number() <= batch_to {
                    Some(envelope.data.clone())
                } else {
                    None
                }
            });

        anyhow::ensure!(
            batches_proven.len() == (batch_to - batch_from + 1) as usize,
            "Fatal error: inconsistent queue state ({} batches between numbers {} and {})",
            batches_proven.len(),
            batch_from,
            batch_to,
        );

        // note: we still hold mutex while verifying the proof -
        // this is desired since we don't want the batches to timeout

        // todo: verify_snark_proof()
        // if false {
        //     anyhow::bail!("proof validation failed")
        // }

        // prove is valid - consuming proven batches
        let consumed_batches_proven: Vec<SignedBatchEnvelope<FriProof>> =
            receiver.try_recv_while(usize::MAX, |envelope| envelope.batch_number() <= batch_to);

        // very unlikely - we just peeked the same batches
        anyhow::ensure!(
            batches_proven.len() == consumed_batches_proven.len(),
            "Fatal error: inconsistency in PeekableReceiver",
        );

        drop(receiver);

        tracing::info!(
            "real SNARK proof for batches {}-{} is accepted",
            batch_from,
            batch_to
        );

        let consumed_batches_proven = consumed_batches_proven
            .into_iter()
            .map(|batch| batch.with_stage(BatchExecutionStage::SnarkProvedReal))
            .collect();

        self.send_downstream(ProofCommand::new(
            consumed_batches_proven,
            SnarkProof::Real(payload),
        ))
        .await?;
        Ok(())
    }

    /// Consumes fake FRI proves from HEAD and turns them into fake SNARKs
    /// Additionally, if `consume_by_timeout` is Some,
    ///    also consumes real FRI proves that are older than `consume_by_timeout`
    async fn consume_fake_proves_from_head(
        &self,
        consume_by_timeout: Option<Duration>,
    ) -> anyhow::Result<()> {
        let consume_if = |envelope: &SignedBatchEnvelope<FriProof>| {
            envelope.data.is_fake()
                || consume_by_timeout
                    .is_some_and(|timeout| envelope.time_since_first_block().unwrap() >= timeout)
        };

        loop {
            let mut receiver = self.committed_batch_receiver.lock().await;
            let batches_with_fake_proofs: Vec<SignedBatchEnvelope<FriProof>> =
                receiver.try_recv_while(self.max_fris_per_snark, consume_if);
            drop(receiver);
            if batches_with_fake_proofs.is_empty() {
                break;
            }

            let real_proofs_count = batches_with_fake_proofs
                .iter()
                .filter(|batch| !batch.data.is_fake())
                .count();

            tracing::info!(
                "generated fake SNARK proof for batches {}-{} ({} real proofs; {} fake proofs)",
                batches_with_fake_proofs.first().unwrap().batch_number(),
                batches_with_fake_proofs.last().unwrap().batch_number(),
                real_proofs_count,
                batches_with_fake_proofs.len() - real_proofs_count,
            );

            // Observability - add traces
            let batches_with_fake_proofs = batches_with_fake_proofs
                .into_iter()
                .map(|batch| batch.with_stage(BatchExecutionStage::SnarkProvedFake))
                .collect();

            self.send_downstream(ProofCommand::new(
                batches_with_fake_proofs,
                SnarkProof::Fake,
            ))
            .await?;
        }
        Ok(())
    }

    async fn send_downstream(&self, proof_command: ProofCommand) -> anyhow::Result<()> {
        self.latency_tracker
            .enter_state(GenericComponentState::WaitingSend);
        self.prove_batches_sender.send(proof_command).await?;
        self.latency_tracker
            .enter_state(GenericComponentState::ProcessingOrWaitingRecv);
        Ok(())
    }

    pub async fn peek_with<R, F>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&SignedBatchEnvelope<FriProof>) -> R,
    {
        self.committed_batch_receiver.lock().await.peek_with(f)
    }
}

const POLL_INTERVAL_MS: u64 = 1000;

pub struct FakeSnarkProver {
    job_manager: Arc<SnarkJobManager>,

    // config
    max_batch_age: Duration,
    polling_interval: Duration,
}

impl FakeSnarkProver {
    pub fn new(job_manager: Arc<SnarkJobManager>, max_batch_age: Duration) -> Self {
        Self {
            job_manager,
            max_batch_age,
            polling_interval: Duration::from_millis(POLL_INTERVAL_MS),
        }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        loop {
            tokio::time::sleep(self.polling_interval).await;
            let head_item_timed_out = self
                .job_manager
                .peek_with(|env| {
                    tracing::debug!(
                        batch_number = env.batch_number(),
                        age = ?env.time_since_first_block(),
                        "checking if head item timed out"
                    );
                    env.time_since_first_block().unwrap() >= self.max_batch_age
                })
                .await
                .unwrap_or(false);
            if head_item_timed_out {
                self.job_manager
                    .consume_fake_proves_from_head(Some(self.max_batch_age))
                    .await?;
            }
        }
    }
}
