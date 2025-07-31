use tokio::sync::mpsc::{Receiver, Sender};
use zksync_os_l1_sender::commands::prove::ProofCommand;
use zksync_os_l1_sender::model::{BatchEnvelope, FriProof, SnarkProof};

/// Primitive SnarkJobManager -
///  groups incoming batches into groups of three;
///  submits fake proof for each such group
pub struct SnarkJobManager {
    // == plumbing ==
    // inbound
    committed_batch_receiver: Receiver<BatchEnvelope<FriProof>>,
    // outbound
    prove_batches_sender: Sender<ProofCommand>,
}

impl SnarkJobManager {
    pub fn new(
        // == plumbing ==
        // inbound
        committed_batch_receiver: Receiver<BatchEnvelope<FriProof>>,
        // outbound
        prove_batches_sender: Sender<ProofCommand>,
    ) -> Self {
        Self {
            committed_batch_receiver,
            prove_batches_sender,
        }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            let batches = self.take_n(2).await?;
            self.prove_batches_sender
                .send(ProofCommand::new(batches, SnarkProof::Fake))
                .await?;
        }
    }

    pub async fn take_n(&mut self, n: usize) -> anyhow::Result<Vec<BatchEnvelope<FriProof>>> {
        let mut out = Vec::default();
        while out.len() < n {
            match self.committed_batch_receiver.recv().await {
                Some(v) => out.push(v),
                None => anyhow::bail!("channel closed"),
            }
        }
        Ok(out)
    }
}
