use crate::model::batches::{BatchEnvelope, FriProof};
use crate::prover_api::proof_storage::ProofStorage;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use zksync_os_l1_sender::L1SenderHandle;

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
    l1sender_handle: L1SenderHandle,
}

impl GaplessCommitter {
    pub fn new(
        next_expected: u64,
        batches_with_proof_receiver: mpsc::Receiver<BatchEnvelope<FriProof>>,
        proof_storage: ProofStorage,
        l1sender_handle: L1SenderHandle,
    ) -> Self {
        GaplessCommitter {
            buffer: Default::default(),
            next_expected,
            batches_with_proof_receiver,
            proof_storage,
            l1sender_handle,
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            match self.batches_with_proof_receiver.recv().await {
                Some(batch) => {
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
            range = format!(
                "{}-{}",
                ready[0].batch_number(),
                ready.last().unwrap().batch_number()
            ),
            "Sending {} ordered batch proofs downstream",
            ready.len()
        );
        for mut batch in ready {
            batch.trace = batch.trace.with_stage("gapless_committer");
            self.proof_storage.save_proof(&batch)?;
            self.l1sender_handle
                .commit(
                    batch.batch.previous_stored_batch_info,
                    batch.batch.commit_batch_info,
                )
                .await?;
        }

        Ok(())
    }
}
