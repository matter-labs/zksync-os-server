use crate::batch_verification_transport::BatchVerificationServer;
use crate::config::BatchVerificationConfig;
use crate::util::tasks::report_exit;
use dashmap::DashMap;
use futures::FutureExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{self, Receiver, Sender};
use zksync_os_batch_verification::BatchVerificationResponse;
use zksync_os_l1_sender::batcher_model::{
    BatchForSigning, BatchSignatureData, SignedBatchEnvelope,
};

pub fn run_pass_batches_without_signing<E>(
    mut batch_for_signing_receiver: Receiver<BatchForSigning<E>>,
    signed_batcher_sender: Sender<SignedBatchEnvelope<E>>,
) -> impl Future<Output = ()> {
    async move {
        while let Some(batch) = batch_for_signing_receiver.recv().await {
            signed_batcher_sender
                .send(batch.with_signatures(BatchSignatureData::NotNeeded))
                .await
                .map_err(|_| anyhow::anyhow!("Failed to send signed batch envelope"))?
        }
        Ok(())
    }
    .map(report_exit::<(), anyhow::Error>(
        "Pass batches without signing",
    ))
}

pub fn run_batch_verification_tasks<E: Send + Sync>(
    config: BatchVerificationConfig,
    batch_for_signing_receiver: Receiver<BatchForSigning<E>>,
    signed_batcher_sender: Sender<SignedBatchEnvelope<E>>,
) -> (
    impl Future<Output = ()>,
    impl Future<Output = ()>,
    impl Future<Output = ()>,
) {
    let (server, response_receiver) = BatchVerificationServer::new();
    let server = Arc::new(server);
    let response_channels = Arc::new(DashMap::new());

    let server_for_fut = server.clone();
    let server_address = config.address.clone();
    let server_fut = async move { server_for_fut.run_server(server_address).await }
        .map(report_exit("Batch verification server"));

    let response_processor_fut = async move {
        run_batch_response_processor(response_receiver, response_channels.clone()).await
    }
    .map(report_exit("Batch response processor"));

    let verifier_fut = async move {
        BatchVerifier::new(config.verification_timeout, config.threshold, server)
            .run(batch_for_signing_receiver, signed_batcher_sender)
            .await
    }
    .map(report_exit("Batch verifier"));

    (server_fut, response_processor_fut, verifier_fut)
}

async fn run_batch_response_processor(
    mut response_receiver: mpsc::Receiver<BatchVerificationResponse>,
    response_channels: Arc<DashMap<u64, mpsc::Sender<BatchVerificationResponse>>>,
) -> anyhow::Result<()> {
    while let Some(response) = response_receiver.recv().await {
        let request_id = response.request_id;

        tracing::debug!(
            "Received batch verification response for request {}",
            request_id
        );

        // Route response to the appropriate channel
        if let Some(sender) = response_channels.get(&request_id) {
            if let Err(e) = sender.send(response).await {
                tracing::warn!(
                    "Failed to route response for request {}: {:?}",
                    request_id,
                    e
                );
            }
        } else {
            // debug, because probably we finished processing this batch and this is an extra response
            tracing::debug!(
                "Received response for unknown request_id {}, dropping",
                request_id
            );
        }
    }

    tracing::info!("Batch response processor shutting down");
    Ok(())
}

pub struct BatchVerifier {
    timeout: Duration,
    required_signatures: usize,
    request_id_counter: AtomicU64,
    server: Arc<BatchVerificationServer>,
    response_channels: Arc<DashMap<u64, mpsc::Sender<BatchVerificationResponse>>>,
}

impl BatchVerifier {
    pub fn new(
        timeout: Duration,
        required_signatures: usize,
        server: Arc<BatchVerificationServer>,
    ) -> Self {
        Self {
            timeout,
            required_signatures,
            request_id_counter: AtomicU64::new(1),
            response_channels: Arc::new(DashMap::new()),
            server,
        }
    }

    async fn run<E: Send + Sync>(
        &self,
        mut batch_for_signing_receiver: Receiver<BatchForSigning<E>>,
        singed_batcher_sender: Sender<SignedBatchEnvelope<E>>,
    ) -> anyhow::Result<()> {
        loop {
            let Some(batch_envelope) = batch_for_signing_receiver.recv().await else {
                // Channel closed, exit the loop
                tracing::info!("BatchForSigning channel closed, exiting verifier",);
                break Ok(());
            };
            let result = self.verify_batch(batch_envelope).await?;
            singed_batcher_sender
                .send(result)
                .await
                .map_err(|_| anyhow::anyhow!("Failed to send signed batch envelope"))?;
        }
    }

    /// Process a batch envelope and collect verification signatures
    async fn verify_batch<E: Send + Sync>(
        &self,
        batch_envelope: BatchForSigning<E>,
    ) -> anyhow::Result<SignedBatchEnvelope<E>> {
        let request_id = self.request_id_counter.fetch_add(1, Ordering::SeqCst);

        tracing::info!(
            "Starting batch verification for batch {} (request {})",
            batch_envelope.batch_number(),
            request_id
        );

        // Create a channel for collecting responses for this request
        let (response_sender, mut response_receiver) =
            mpsc::channel::<BatchVerificationResponse>(100);

        // Register the channel for this request_id
        self.response_channels.insert(request_id, response_sender);

        // Send verification request to all connected clients
        self.server
            .send_verification_request(&batch_envelope, request_id)
            .await?;

        // Collect responses with timeout
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + self.timeout;

        loop {
            let remaining_time = deadline - tokio::time::Instant::now();
            if remaining_time <= Duration::from_secs(0) {
                anyhow::bail!("Timeout");
            }

            match tokio::time::timeout(remaining_time, response_receiver.recv()).await {
                Ok(Some(response)) => {
                    // TODO add validation of signatures incl. uniqueness
                    responses.push(response.signature);
                    if responses.len() >= self.required_signatures {
                        break;
                    }
                }
                Ok(None) => anyhow::bail!("Channel closed"),
                Err(_) => anyhow::bail!("Timeout"),
            }
        }

        // Cleanup: remove the channel for this request_id
        self.response_channels.remove(&request_id);

        tracing::info!(
            "Collected {} verification responses for batch {} (request {})",
            responses.len(),
            batch_envelope.batch_number(),
            request_id
        );

        Ok(batch_envelope.with_signatures(BatchSignatureData::Signed {
            signatures: responses,
        }))
    }
}
