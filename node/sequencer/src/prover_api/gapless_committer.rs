use crate::prover_api::proof_storage::ProofStorage;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, FriProof};
use zksync_os_l1_sender::commands::commit::CommitCommand;
use zksync_os_observability::{ComponentStateLatencyTracker, GenericComponentState};

/// Receives Batches with proofs - potentially out of order;
/// Fixes the order (by filling in the `buffer` field);  
/// Sends batches downstream:
///   * First to the `proof_storage`
///   * Then to the `l1sender_handle`
///
#[derive(Debug)]
pub struct GaplessCommitter {
    // == state ==
    buffer: BTreeMap<u64, BatchEnvelope<FriProof>>,
    next_expected: u64,

    // == plumbing ==
    // inbound
    batches_with_proof_receiver: mpsc::Receiver<BatchEnvelope<FriProof>>,
    // outbound
    proof_storage: ProofStorage,
    commit_batch_sender: mpsc::Sender<CommitCommand>,
    latency_tracker: ComponentStateLatencyTracker,
}

impl GaplessCommitter {
    pub fn new(
        next_expected: u64,
        batches_with_proof_receiver: mpsc::Receiver<BatchEnvelope<FriProof>>,
        proof_storage: ProofStorage,
        commit_batch_sender: mpsc::Sender<CommitCommand>,
    ) -> Self {
        let latency_tracker = ComponentStateLatencyTracker::new(
            "gapless_committer",
            GenericComponentState::WaitingRecv,
            None,
        );
        GaplessCommitter {
            buffer: Default::default(),
            next_expected,
            batches_with_proof_receiver,
            proof_storage,
            commit_batch_sender,
            latency_tracker,
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            self.latency_tracker
                .enter_state(GenericComponentState::WaitingRecv);
            match self.batches_with_proof_receiver.recv().await {
                Some(batch) => {
                    self.latency_tracker
                        .enter_state(GenericComponentState::Processing);
                    self.buffer.insert(batch.batch_number(), batch);
                    self.flush_ready().await?;
                }
                None => {
                    anyhow::bail!("channel unexpectedly closed");
                }
            }
        }
    }
    async fn flush_ready(&mut self) -> anyhow::Result<()> {
        let mut ready: Vec<BatchEnvelope<FriProof>> = Vec::default();
        while let Some(next_batch) = self.buffer.remove(&self.next_expected) {
            ready.push(next_batch);
            self.next_expected += 1;
        }

        if ready.is_empty() {
            return Ok(());
        }

        tracing::info!(
            buffer_size = self.buffer.len(),
            "Sending {} (batches {}-{}) ordered batch proofs downstream",
            ready.len(),
            ready[0].batch_number(),
            ready.last().unwrap().batch_number()
        );
        for batch in ready {
            let batch = batch.with_stage(BatchExecutionStage::FriProofStored);
            self.proof_storage.save_proof(&batch)?;
            self.latency_tracker
                .enter_state(GenericComponentState::WaitingSend);
            self.commit_batch_sender
                .send(CommitCommand::new(batch))
                .await?;
            self.latency_tracker
                .enter_state(GenericComponentState::Processing);
        }

        Ok(())
    }
}
