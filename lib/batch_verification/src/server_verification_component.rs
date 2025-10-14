use crate::config::BatchVerificationConfig;
use crate::{BatchVerificationRequestError, BatchVerificationServer};
use crate::{BatchVerificationResponse, BatchVerificationResult};
use async_trait::async_trait;
use dashmap::DashMap;
use futures::FutureExt;
use futures::future::join_all;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{self, Sender};
use tokio::time::Instant;
use zksync_os_l1_sender::batcher_model::{
    BatchForSigning, BatchSignatureData, SignedBatchEnvelope,
};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_types::BatchSignatureSet;

fn report_exit<T, E: std::fmt::Debug>(name: &'static str) -> impl Fn(Result<T, E>) {
    move |result| match result {
        Ok(_) => tracing::warn!("{name} unexpectedly exited"),
        Err(e) => tracing::error!("{name} failed: {e:#?}"),
    }
}
pub struct BatchVerificationPipelineStep<E> {
    config: BatchVerificationConfig,
    _phantom: std::marker::PhantomData<E>,
}

impl<E> BatchVerificationPipelineStep<E> {
    pub fn new(config: BatchVerificationConfig) -> Self {
        Self {
            config,
            _phantom: std::marker::PhantomData,
        }
    }
}

#[async_trait]
impl<E: Send + Sync + 'static> PipelineComponent for BatchVerificationPipelineStep<E> {
    type Input = BatchForSigning<E>;
    type Output = SignedBatchEnvelope<E>;

    const NAME: &'static str = "batch_verification";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        if self.config.enabled {
            let (server, response_receiver) = BatchVerificationServer::new();
            let server = Arc::new(server);
            let response_channels = Arc::new(DashMap::new());

            let server_for_fut = server.clone();
            let server_address = self.config.address.clone();
            let server_fut = async move { server_for_fut.run_server(server_address).await }
                .boxed()
                .map(report_exit("Batch verification server"));

            let response_channels_for_fut = response_channels.clone();
            let response_processor_fut = async move {
                run_batch_response_processor(response_receiver, response_channels_for_fut).await
            }
            .boxed()
            .map(report_exit("Batch response processor"));

            let verifier_fut = async move {
                BatchVerifier::new(self.config, response_channels, server)
                    .run(input, output)
                    .await
            }
            .boxed()
            .map(report_exit("Batch verifier"));

            join_all(vec![server_fut, response_processor_fut, verifier_fut]).await;
            Ok(())
        } else {
            while let Some(batch) = input.recv().await {
                output
                    .send(batch.with_signatures(BatchSignatureData::NotNeeded))
                    .await
                    .map_err(|_| anyhow::anyhow!("Failed to send signed batch envelope"))?
            }
            Ok(())
        }
    }
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
            tracing::debug!("Response for unknown request_id {}, dropping", request_id);
        }
    }

    tracing::info!("Batch response processor shutting down");
    Ok(())
}

/// Processes batches without signatures by broadcasting signing requests to all
/// connected signer ENs. When enough signatures are collected it added signatures
/// to the batch and sends it to the next component. If not enough signatures are
/// collected within the timeout, signing requests are resend. More ENs maybe
/// available on next attempt or already connected ENs may now be able to verify
/// the batch. IDs are used to correlate requests and responses.
struct BatchVerifier {
    config: BatchVerificationConfig,
    request_id_counter: AtomicU64,
    server: Arc<BatchVerificationServer>,
    response_channels: Arc<DashMap<u64, mpsc::Sender<BatchVerificationResponse>>>,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Timeout")]
    Timeout,
    #[error("Not enough signers")]
    NotEnoughSigners,
    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<BatchVerificationRequestError> for BatchVerificationError {
    fn from(err: BatchVerificationRequestError) -> Self {
        match err {
            BatchVerificationRequestError::NotEnoughClients => {
                BatchVerificationError::NotEnoughSigners
            }
            BatchVerificationRequestError::SendError(e) => {
                BatchVerificationError::Internal(e.to_string())
            }
        }
    }
}

impl BatchVerificationError {
    fn retryable(&self) -> bool {
        !matches!(self, BatchVerificationError::Internal(_))
    }
}

impl BatchVerifier {
    pub fn new(
        config: BatchVerificationConfig,
        response_channels: Arc<DashMap<u64, mpsc::Sender<BatchVerificationResponse>>>,
        server: Arc<BatchVerificationServer>,
    ) -> Self {
        Self {
            config,
            request_id_counter: AtomicU64::new(1),
            response_channels,
            server,
        }
    }

    async fn run<E: Send + Sync>(
        &self,
        mut batch_for_signing_receiver: PeekableReceiver<BatchForSigning<E>>,
        singed_batcher_sender: Sender<SignedBatchEnvelope<E>>,
    ) -> anyhow::Result<()> {
        loop {
            // We process the batches one by one. Consider adding concurrency here when we need it.
            let Some(batch_envelope) = batch_for_signing_receiver.recv().await else {
                // Channel closed, exit the loop
                tracing::info!("BatchForSigning channel closed, exiting verifier",);
                break Ok(());
            };
            let mut retry_count = 0;
            let deadline = Instant::now() + self.config.total_timeout;
            let signatures = loop {
                match self
                    .collect_batch_verification_signatures(&batch_envelope)
                    .await
                {
                    Ok(result) => break Ok(result),
                    Err(err) if err.retryable() => {
                        if Instant::now() < deadline {
                            retry_count += 1;
                            tracing::warn!(
                                "Batch verification failed, attempt {} retrying. Error: {}",
                                retry_count,
                                err
                            );

                            tokio::time::sleep(self.config.retry_delay).await;
                        } else {
                            tracing::warn!(
                                "Batch verification failed after {} retries exceeding total timeout. Bailing. Last error: {}",
                                retry_count,
                                err
                            );
                            break Err(err);
                        }
                    }
                    Err(err) => {
                        tracing::warn!("Batch verification failed. Non retryable error: {}", err);
                        break Err(err);
                    }
                }
            }?;
            singed_batcher_sender
                .send(batch_envelope.with_signatures(BatchSignatureData::Signed { signatures }))
                .await
                .map_err(|_| anyhow::anyhow!("Failed to send signed batch envelope"))?;
        }
    }

    /// Process a batch envelope and collect verification signatures
    async fn collect_batch_verification_signatures<E: Send + Sync>(
        &self,
        batch_envelope: &BatchForSigning<E>,
    ) -> Result<BatchSignatureSet, BatchVerificationError> {
        let request_id = self.request_id_counter.fetch_add(1, Ordering::SeqCst);

        tracing::info!(
            "Starting batch verification for batch {} (request {})",
            batch_envelope.batch_number(),
            request_id
        );

        // Create a channel for collecting responses for this request
        let (response_sender, mut response_receiver) =
            mpsc::channel::<BatchVerificationResponse>(self.config.threshold);

        // Register the channel for this request_id
        self.response_channels.insert(request_id, response_sender);

        // Send verification request to all connected clients
        self.server
            .send_verification_request(batch_envelope, request_id, self.config.threshold)
            .await?;

        // Collect responses with timeout
        let mut responses = BatchSignatureSet::new();
        let deadline = Instant::now() + self.config.request_timeout;

        loop {
            let remaining_time = deadline - Instant::now();
            if remaining_time <= Duration::from_secs(0) {
                return Err(BatchVerificationError::Timeout);
            }

            match tokio::time::timeout(remaining_time, response_receiver.recv()).await {
                Ok(Some(BatchVerificationResponse {
                    result: BatchVerificationResult::Success(signature),
                    ..
                })) => {
                    // TODO add validation of signatures incl. uniqueness
                    responses.push(signature);
                    tracing::debug!(
                        "Validated response for batch {} (request {}), {} of {}",
                        batch_envelope.batch_number(),
                        request_id,
                        responses.len(),
                        self.config.threshold
                    );
                    if responses.len() >= self.config.threshold {
                        break;
                    }
                }
                Ok(Some(BatchVerificationResponse {
                    result: BatchVerificationResult::Refused(reason),
                    ..
                })) => {
                    tracing::info!(
                        "Verification refused for batch {} (request {}): {}",
                        batch_envelope.batch_number(),
                        request_id,
                        reason
                    );
                }
                Ok(None) => {
                    return Err(BatchVerificationError::Internal(
                        "Channel closed".to_string(),
                    ));
                }
                Err(_) => return Err(BatchVerificationError::Timeout),
            }
        }

        // Cleanup: remove the channel for this request_id
        self.response_channels.remove(&request_id);

        tracing::info!(
            "Collected enough ({}) verification responses for batch {} (request {})",
            responses.len(),
            batch_envelope.batch_number(),
            request_id
        );

        Ok(responses)
    }
}
