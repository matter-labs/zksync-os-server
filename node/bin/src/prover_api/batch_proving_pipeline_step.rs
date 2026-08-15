use super::proof_storage::ProofStorage;
use crate::prover_api::fri_job_manager::FriJobManager;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_batch_types::batcher_model::{
    BatchEnvelope, FriProof, ProverInput, ProvingInputs, SignedBatchEnvelope,
};
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};

/// The per-batch proving stage: a batch leaves it once every proof system
/// that must prove the batch itself has done so.
///
/// The stage is named for its level rather than for a proof system: today it
/// drives the Airbender FRI lane, and a second proof system proving each batch
/// belongs here too.
///
/// This component:
/// - Receives batches with ProverInput from the batcher
/// - Adds them directly to FriJobManager (which makes them available via HTTP API)
/// - Receives proofs from FriJobManager (submitted via HTTP API or fake provers)
/// - Forwards the proofs downstream in the pipeline
///
/// The FriJobManager itself is purely reactive (no run loop), accessed/driven by:
/// - HTTP server (provers call pick_next_job, submit_proof, etc.)
/// - Fake provers pool
/// - This pipeline step (adds jobs via add_job)
pub struct BatchProvingPipelineStep {
    last_proved_batch_number: u64,
    fri_job_manager: Arc<FriJobManager>,
    batches_with_proof_receiver: mpsc::Receiver<SignedBatchEnvelope<FriProof>>,
    /// The second proof system's per-batch lane, when it is enabled.
    zisk_job_manager: Option<Arc<zisk_prover_lane::ZiskJobManager>>,
    /// Batches whose second proof has just been accepted.
    zisk_batch_ready: Option<mpsc::Receiver<u64>>,
    /// The commit gate: when set, a batch leaves this stage only once every
    /// proof system at this level has proved it, and admission is bounded to
    /// this many incomplete batches.
    gate: Option<CommitGateConfig>,
}

/// Settings of the per-batch commit gate.
#[derive(Debug, Clone, Copy)]
pub struct CommitGateConfig {
    /// How far ahead of the oldest incomplete batch this stage may admit.
    /// Reaching it stops admission, which is what stalls block production
    /// while a proof system is unavailable — the accepted trade for gating
    /// commits on both proofs.
    pub admission_window: u64,
}

/// What the stage is still holding for batches it has admitted.
#[derive(Default)]
struct GateState {
    /// Admitted, not yet forwarded. Ordered so the oldest is the window's
    /// anchor.
    admitted: std::collections::BTreeSet<u64>,
    /// Primary proof in hand, waiting for the second.
    awaiting_second: std::collections::HashMap<u64, SignedBatchEnvelope<FriProof>>,
    /// Second proof in hand, waiting for the primary.
    second_proved: std::collections::HashSet<u64>,
}

enum SecondProofArrival {
    /// The batch was already released (or was never admitted). A stale or
    /// duplicate notice must not recreate gate state.
    Ignored,
    /// The second proof arrived first and is recorded for the primary proof.
    Waiting,
    /// Both proofs are present and the batch can leave the gate.
    Ready(Box<SignedBatchEnvelope<FriProof>>),
}

impl GateState {
    /// Whether `batch_number` may be admitted without exceeding the window.
    fn admits(&self, batch_number: u64, window: u64) -> bool {
        self.admitted
            .first()
            .is_none_or(|oldest| batch_number.saturating_sub(*oldest) < window)
    }

    fn note_second_proof(&mut self, batch_number: u64) -> SecondProofArrival {
        if !self.admitted.contains(&batch_number) {
            return SecondProofArrival::Ignored;
        }
        if let Some(proof) = self.awaiting_second.remove(&batch_number) {
            self.admitted.remove(&batch_number);
            SecondProofArrival::Ready(Box::new(proof))
        } else {
            self.second_proved.insert(batch_number);
            SecondProofArrival::Waiting
        }
    }
}

impl BatchProvingPipelineStep {
    pub fn new(
        proof_storage: ProofStorage,
        last_proved_batch_number: u64,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
    ) -> (Self, Arc<FriJobManager>) {
        Self::new_with_zisk(
            proof_storage,
            last_proved_batch_number,
            assignment_timeout,
            max_assigned_batch_range,
            SecondProofBatchStage::Disabled,
        )
    }

    pub fn new_with_zisk(
        proof_storage: ProofStorage,
        last_proved_batch_number: u64,
        assignment_timeout: Duration,
        max_assigned_batch_range: usize,
        second_proof: SecondProofBatchStage,
    ) -> (Self, Arc<FriJobManager>) {
        // Create channel for completed proofs - between FriProveManager and GaplessCommitter
        let (batches_with_proof_sender, batches_with_proof_receiver) =
            mpsc::channel::<SignedBatchEnvelope<FriProof>>(5);

        let fri_job_manager = Arc::new(FriJobManager::new(
            batches_with_proof_sender,
            proof_storage,
            assignment_timeout,
            max_assigned_batch_range,
        ));

        // The gate, the lane that feeds it and the channel that releases it
        // arrive together or not at all — a gate without the other two would
        // hold every batch forever.
        let (zisk_job_manager, zisk_batch_ready, gate) = match second_proof {
            SecondProofBatchStage::Disabled => (None, None, None),
            SecondProofBatchStage::Shadow { manager } => (Some(manager), None, None),
            SecondProofBatchStage::Required {
                manager,
                ready,
                gate,
            } => (Some(manager), Some(ready), Some(gate)),
        };

        let result = Self {
            last_proved_batch_number,
            fri_job_manager: fri_job_manager.clone(),
            batches_with_proof_receiver,
            zisk_job_manager,
            zisk_batch_ready,
            gate,
        };

        (result, fri_job_manager)
    }
}

/// What the per-batch stage does about a second proof system.
///
/// One value instead of a manager, a gate and a receiver that could be
/// combined wrongly: `Required` carries everything holding a batch back
/// requires, so a gate that nothing can release is not constructable.
pub enum SecondProofBatchStage {
    Disabled,
    /// The lane proves every batch; the stage forwards on the primary proof.
    Shadow {
        manager: Arc<zisk_prover_lane::ZiskJobManager>,
    },
    /// A batch's data may not be committed before both systems have proved it.
    Required {
        manager: Arc<zisk_prover_lane::ZiskJobManager>,
        /// Announced by the lane when it accepts a batch's proof.
        ready: mpsc::Receiver<u64>,
        gate: CommitGateConfig,
    },
}

/// How many ready-batch announcements may queue before the second lane waits
/// for room. It never drops one: this is the only notice the gate gets.
pub const MAX_READY_BATCH_SIGNALS: usize = 64;

/// The half of the stage that admits a sealed batch into the proving lanes.
struct BatchAdmission {
    fri_job_manager: Arc<FriJobManager>,
    zisk_job_manager: Option<Arc<zisk_prover_lane::ZiskJobManager>>,
}

impl BatchAdmission {
    /// Admit a batch into every lane of this stage.
    async fn admit(&self, batch: SignedBatchEnvelope<ProvingInputs>) {
        let batch = self.open_second_proof_job(batch).await;
        // Adding to the Airbender lane awaits while its queue is full, which is
        // the stage's backpressure onto the batcher.
        self.fri_job_manager.add_job(batch).await;
    }

    /// Open the second proof system's job for this batch, and hand back the
    /// envelope carrying only what the Airbender lane needs.
    ///
    /// The witness was built when the batch sealed and rode the envelope here,
    /// so the batcher never had to reach into a proving lane. `add_job` does
    /// not block the pipeline: a full active queue parks the input in the
    /// lane's bounded backlog and promotes it when a slot frees.
    async fn open_second_proof_job(
        &self,
        batch: SignedBatchEnvelope<ProvingInputs>,
    ) -> SignedBatchEnvelope<ProverInput> {
        let ProvingInputs { fri, second_proof } = batch.data;
        let batch = BatchEnvelope {
            batch: batch.batch,
            data: fri,
            signature_data: batch.signature_data,
            latency_tracker: batch.latency_tracker,
        };

        if let (Some(zisk_job_manager), Some(input)) =
            (self.zisk_job_manager.as_ref(), second_proof)
        {
            let batch_number = batch.batch_number();
            tracing::info!(
                batch_number,
                zisk_bytes = input.bytes.len(),
                "opening the second proof system's job from the sealed batch"
            );
            zisk_job_manager
                .add_job(
                    batch_number,
                    zisk_prover_lane::ZiskJobData {
                        zisk_data: input.bytes,
                        batch_metadata: batch.batch.clone(),
                        added_at: std::time::Instant::now(),
                        seal_shadow_commitment: input.seal_commitment,
                    },
                )
                .await;
        }
        batch
    }
}

#[async_trait]
impl PipelineComponent for BatchProvingPipelineStep {
    type Input = SignedBatchEnvelope<ProvingInputs>;
    type Output = SignedBatchEnvelope<FriProof>;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::FriJobManager;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // Split the state the two concurrent halves touch: the inbound half
        // opens jobs, the outbound half owns the completion receiver.
        let Self {
            last_proved_batch_number,
            fri_job_manager,
            mut batches_with_proof_receiver,
            zisk_job_manager,
            mut zisk_batch_ready,
            gate,
        } = self;
        let admission = BatchAdmission {
            fri_job_manager: fri_job_manager.clone(),
            zisk_job_manager,
        };
        // The two halves below run concurrently — they must, because admitting
        // a batch can block on a full job queue, and the queue only drains as
        // completions are forwarded. The gate makes the admitting half depend
        // on the forwarding half's state, so that state is shared rather than
        // owned by either.
        let state = Arc::new(tokio::sync::Mutex::new(GateState::default()));
        let released = Arc::new(tokio::sync::Notify::new());

        tokio::select! {
            result = async {
                while let Some(batch) = input.recv_and_record_picked(&state_reporter).await {
                    if batch.batch_number() > last_proved_batch_number {
                        let batch_number = batch.batch_number();
                        if let Some(gate) = gate {
                            // Wait for room rather than admitting past the
                            // window: this is where a stalled proof system
                            // stops block production, via the batcher's own
                            // backpressure.
                            loop {
                                // Register for the wake-up BEFORE testing the
                                // window. `notify_waiters` stores no permit, so
                                // a release landing between the test and the
                                // await would otherwise be lost — and if it was
                                // the last one, admission would wait forever
                                // with room to spare.
                                let notified = released.notified();
                                tokio::pin!(notified);
                                notified.as_mut().enable();
                                if state
                                    .lock()
                                    .await
                                    .admits(batch_number, gate.admission_window)
                                {
                                    break;
                                }
                                tracing::info!(
                                    batch_number,
                                    window = gate.admission_window,
                                    "per-batch proving stage is at its admission window; \
                                     waiting for an older batch to finish proving"
                                );
                                notified.await;
                            }
                            state.lock().await.admitted.insert(batch_number);
                        }
                        tracing::info!(batch_number, "received batch for per-batch proving");
                        // Split the batch's proving inputs across the lanes of
                        // this stage. The second proof system's bytes stop here:
                        // nothing downstream carries them.
                        admission.admit(batch).await;
                    } else {
                        // Already proven - send with fake proof to pass through the pipeline
                        let batch_with_fake_proof =
                            batch.with_data(FriProof::AlreadySubmittedToL1);
                        output.send_and_record(batch_with_fake_proof, &state_reporter)?;
                    }
                }
                Ok::<(), anyhow::Error>(())
            } => {
                result?;
                tracing::info!("inbound channel closed");
                return Ok(());
            },
            result = async {
                // Without the gate this stage forwards what the primary lane
                // proved, exactly as it always has: no second proof to wait
                // for, so no join and nothing to hold.
                let Some(gate) = gate else {
                    while let Some(proof) = batches_with_proof_receiver.recv().await {
                        tracing::info!(
                            "Received batch after FRI proving: {:?}",
                            proof.batch_number()
                        );
                        output.send_and_record(proof, &state_reporter)?;
                    }
                    return Ok::<(), anyhow::Error>(());
                };
                loop {
                    tokio::select! {
                        proof = batches_with_proof_receiver.recv() => {
                            let Some(proof) = proof else { break };
                            let batch_number = proof.batch_number();
                            let mut state = state.lock().await;
                            if state.second_proved.remove(&batch_number) {
                                state.admitted.remove(&batch_number);
                                drop(state);
                                released.notify_waiters();
                                output.send_and_record(proof, &state_reporter)?;
                            } else {
                                tracing::info!(
                                    batch_number,
                                    window = gate.admission_window,
                                    "batch is proved by the primary lane and is waiting for \
                                     its second proof before its data may be committed"
                                );
                                state.awaiting_second.insert(batch_number, proof);
                            }
                        }
                        ready = async {
                            match zisk_batch_ready.as_mut() {
                                Some(receiver) => receiver.recv().await,
                                // The gate is only armed with the lane that
                                // feeds it, so this is unreachable; never
                                // resolve rather than spin.
                                None => std::future::pending().await,
                            }
                        } => {
                            let Some(batch_number) = ready else { break };
                            let mut state = state.lock().await;
                            match state.note_second_proof(batch_number) {
                                SecondProofArrival::Ignored => tracing::debug!(
                                    batch_number,
                                    "ignoring a duplicate or stale second-proof notice"
                                ),
                                SecondProofArrival::Waiting => {}
                                SecondProofArrival::Ready(proof) => {
                                    drop(state);
                                    released.notify_waiters();
                                    output.send_and_record(*proof, &state_reporter)?;
                                }
                            }
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            } => {
                result?;
                tracing::info!("outbound channel closed");
                return Ok(());
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(admitted: &[u64]) -> GateState {
        GateState {
            admitted: admitted.iter().copied().collect(),
            ..Default::default()
        }
    }

    #[test]
    fn readiness_for_a_released_batch_is_ignored() {
        let mut state = GateState::default();

        assert!(matches!(
            state.note_second_proof(7),
            SecondProofArrival::Ignored
        ));
        assert!(
            state.second_proved.is_empty(),
            "a duplicate notice after release must not leak gate state"
        );
    }

    /// The window is measured from the oldest batch the stage still holds, so
    /// one stuck batch stops admission instead of letting the stage run away
    /// from it. Batches completing out of order move the anchor only when the
    /// oldest one completes.
    #[test]
    fn the_window_is_anchored_at_the_oldest_incomplete_batch() {
        assert!(state_with(&[]).admits(100, 4), "nothing in flight");

        let stuck = state_with(&[100]);
        assert!(stuck.admits(103, 4), "three ahead of the anchor still fits");
        assert!(!stuck.admits(104, 4), "four ahead is past the window");
        assert!(!stuck.admits(200, 4));

        let moved_on = state_with(&[105]);
        assert!(moved_on.admits(108, 4), "the anchor moved with the oldest");
    }

    /// A window of one admits the anchor itself and nothing beyond it.
    #[test]
    fn a_window_of_one_admits_only_the_anchor() {
        let state = state_with(&[7]);
        assert!(state.admits(7, 1));
        assert!(!state.admits(8, 1));
    }
}
