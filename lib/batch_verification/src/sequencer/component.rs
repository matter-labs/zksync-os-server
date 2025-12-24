use super::metrics::BATCH_VERIFICATION_SEQUENCER_METRICS;
use super::server::{BatchVerificationRequestError, BatchVerificationServer};
use crate::config::BatchVerificationConfig;
use crate::{BatchVerificationResponse, BatchVerificationResult};
use alloy::primitives::Address;
use async_trait::async_trait;
use futures::FutureExt;
use futures::future::select_all;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::mpsc::{self, Sender};
use tokio::time::Instant;
use zksync_os_batch_types::{BatchSignatureSet, ValidatedBatchSignature};
use zksync_os_contract_interface::models::CommitBatchInfo;
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{
    BatchForSigning, BatchSignatureData, SignedBatchEnvelope,
};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

fn report_exit<T, E: std::fmt::Debug>(name: &'static str) -> impl Fn(Result<T, E>) {
    move |result| match result {
        Ok(_) => tracing::warn!("{name} unexpectedly exited"),
        Err(e) => tracing::error!("{name} failed: {e:#?}"),
    }
}
pub struct BatchVerificationPipelineStep<E> {
    config: BatchVerificationConfig,
    last_committed_batch_number: u64,
    _phantom: std::marker::PhantomData<E>,
}

impl<E> BatchVerificationPipelineStep<E> {
    pub fn new(config: BatchVerificationConfig, last_committed_batch_number: u64) -> Self {
        Self {
            config,
            last_committed_batch_number,
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
        if self.config.server_enabled {
            let (server, response_receiver) = BatchVerificationServer::new();
            let server = Arc::new(server);
            // Stores response channels for each request ID to route responses
            // depending on request id. Allows collect_batch_verification_signatures
            // to be run concurrently. Unimplemented currently.
            let response_channels = Arc::new(RwLock::new(HashMap::new()));

            let server_for_fut = server.clone();
            let server_address = self.config.listen_address.clone();
            let server_fut = server_for_fut
                .run_server(server_address)
                .boxed()
                .map(report_exit("Batch verification server"));

            let response_channels_for_fut = response_channels.clone();
            let response_processor_fut =
                run_batch_response_processor(response_receiver, response_channels_for_fut)
                    .boxed()
                    .map(report_exit("Batch response processor"));

            let verifier = BatchVerifier::new(
                self.config,
                response_channels,
                server,
                self.last_committed_batch_number,
            );
            let verifier_fut = verifier
                .run(input, output)
                .boxed()
                .map(report_exit("Batch verifier"));

            select_all(vec![server_fut, response_processor_fut, verifier_fut]).await;
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

/// Takes BatchVerificationResponse from server and routes them to appropriate
/// per-request id channels. The full flow is:
/// BatchVerificationServer -> response_receiver -> run_batch_response_processor ->
/// -> response_channels -> BatchVerifier::collect_batch_verification_signatures
async fn run_batch_response_processor(
    mut response_receiver: mpsc::Receiver<BatchVerificationResponse>,
    response_channels: Arc<RwLock<HashMap<u64, mpsc::Sender<BatchVerificationResponse>>>>,
) -> anyhow::Result<()> {
    let latency_tracker = ComponentStateReporter::global().handle_for(
        "batch_response_processor",
        GenericComponentState::WaitingRecv,
    );
    while let Some(response) = response_receiver.recv().await {
        latency_tracker.enter_state(GenericComponentState::Processing);
        let request_id = response.request_id;

        // Route response to the appropriate channel
        if let Some(sender) = response_channels.read().await.get(&request_id) {
            tracing::debug!(request_id, "Received batch verification response");
            latency_tracker.enter_state(GenericComponentState::WaitingSend);
            if let Err(e) = sender.send(response).await {
                tracing::warn!(request_id, ?e, "Failed to route response");
            }
        } else {
            // debug, because probably we finished processing this batch and this is an extra response
            tracing::debug!(request_id, "Response for unknown request_id, dropping");
        }
        latency_tracker.enter_state(GenericComponentState::WaitingRecv);
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
    accepted_signers: Vec<Address>,
    request_id_counter: AtomicU64,
    server: Arc<BatchVerificationServer>,
    response_channels: Arc<RwLock<HashMap<u64, mpsc::Sender<BatchVerificationResponse>>>>,
    last_committed_batch_number: u64,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Timeout")]
    Timeout,
    #[error("Not enough signers: {0} < {1}")]
    NotEnoughSigners(usize, usize),
    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<BatchVerificationRequestError> for BatchVerificationError {
    fn from(err: BatchVerificationRequestError) -> Self {
        match err {
            BatchVerificationRequestError::NotEnoughClients(clients_count, required_clients) => {
                BatchVerificationError::NotEnoughSigners(clients_count, required_clients)
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
        response_channels: Arc<RwLock<HashMap<u64, mpsc::Sender<BatchVerificationResponse>>>>,
        server: Arc<BatchVerificationServer>,
        last_committed_batch_number: u64,
    ) -> Self {
        let accepted_signers = config
            .accepted_signers
            .clone()
            .into_iter()
            .map(|s| s.parse().unwrap())
            .collect();
        Self {
            config,
            request_id_counter: AtomicU64::new(1),
            response_channels,
            server,
            accepted_signers,
            last_committed_batch_number,
        }
    }

    async fn run<E: Send + Sync>(
        &self,
        mut batch_for_signing_receiver: PeekableReceiver<BatchForSigning<E>>,
        singed_batcher_sender: Sender<SignedBatchEnvelope<E>>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global()
            .handle_for("batch_verifier", GenericComponentState::WaitingRecv);
        let metrics = &*BATCH_VERIFICATION_SEQUENCER_METRICS;

        loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            // We process the batches one by one. Consider adding concurrency here when we need it.
            let Some(batch_envelope) = batch_for_signing_receiver.recv().await else {
                // Channel closed, exit the loop
                tracing::info!("BatchForSigning channel closed, exiting verifier",);
                break Ok(());
            };

            // We skip signing batches that were already committed. This happens on startup
            if batch_envelope.batch_number() <= self.last_committed_batch_number {
                tracing::info!(
                    "Skipping signing of already committed batch {}",
                    batch_envelope.batch_number()
                );
                singed_batcher_sender
                    .send(
                        batch_envelope
                            .with_signatures(BatchSignatureData::AlreadyCommitted)
                            .with_stage(BatchExecutionStage::BatchSigned),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("Failed to send signed batch envelope"))?;
                continue;
            }

            latency_tracker.enter_state(GenericComponentState::Processing);
            let batch_envelope = batch_envelope.with_stage(BatchExecutionStage::SigningStarted);
            metrics.last_batch_number.set(batch_envelope.batch_number());

            let mut retry_count = 0;
            let deadline = Instant::now() + self.config.total_timeout;
            let start_time = Instant::now();
            let signatures = loop {
                match self
                    .collect_batch_verification_signatures(&batch_envelope, retry_count + 1)
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

            metrics.attempts_to_success.observe(retry_count + 1);
            metrics.total_latency.observe(start_time.elapsed());

            latency_tracker.enter_state(GenericComponentState::WaitingSend);
            singed_batcher_sender
                .send(
                    batch_envelope
                        .with_signatures(BatchSignatureData::Signed { signatures })
                        .with_stage(BatchExecutionStage::BatchSigned),
                )
                .await
                .map_err(|_| anyhow::anyhow!("Failed to send signed batch envelope"))?;
        }
    }

    /// Process a batch envelope and collect verification signatures.
    /// We discard collected signatures if not enough are collected. If a node
    /// has signed a request once, it will sign the same batch again,
    /// so it's safe to discard.
    async fn collect_batch_verification_signatures<E: Send + Sync>(
        &self,
        batch_envelope: &BatchForSigning<E>,
        attempt_number: u64,
    ) -> Result<BatchSignatureSet, BatchVerificationError> {
        let metrics = &*BATCH_VERIFICATION_SEQUENCER_METRICS;
        let request_id = self.request_id_counter.fetch_add(1, Ordering::SeqCst);
        metrics.last_request_id.set(request_id);

        tracing::info!(
            batch_number = batch_envelope.batch_number(),
            request_id = request_id,
            "Starting batch verification",
        );

        // Create a channel for collecting responses for this request
        let (response_sender, mut response_receiver) =
            mpsc::channel::<BatchVerificationResponse>(self.config.threshold);

        // Register the channel for this request_id
        self.response_channels
            .write()
            .await
            .insert(request_id, response_sender);

        // Send verification request to all connected clients
        self.server
            .send_verification_request(batch_envelope, request_id, self.config.threshold)
            .await?;

        let commit_data = batch_envelope.batch.batch_info.commit_info.clone();

        // Collect responses with timeout
        let mut responses = BatchSignatureSet::new();
        let start_time = Instant::now();
        let deadline = Instant::now() + self.config.request_timeout;

        loop {
            let remaining_time = deadline - Instant::now();
            if remaining_time <= Duration::from_secs(0) {
                return Err(BatchVerificationError::Timeout);
            }

            let response =
                match tokio::time::timeout(remaining_time, response_receiver.recv()).await {
                    Ok(Some(response)) => response,
                    Ok(None) => {
                        return Err(BatchVerificationError::Internal(
                            "Channel closed".to_string(),
                        ));
                    }
                    Err(_) => return Err(BatchVerificationError::Timeout),
                };

            let Some(validated_signature) =
                self.process_response(&commit_data, request_id, response)
            else {
                continue;
            };

            let latency = start_time.elapsed();
            let signer = validated_signature.signer().to_string();

            metrics.per_signer_latency[&signer].observe(latency);
            metrics.successful_attempt_per_signer[&signer].observe(attempt_number);

            if responses.push(validated_signature).is_err() {
                tracing::warn!(
                    batch_number = batch_envelope.batch_number(),
                    request_id = request_id,
                    signer = signer,
                    "Received duplicated signature",
                );
                continue;
            }

            tracing::debug!(
                batch_number = batch_envelope.batch_number(),
                request_id = request_id,
                signer = signer,
                response_latency_ms = latency.as_millis() as u64,
                "Validated response {} of {}",
                responses.len(),
                self.config.threshold
            );

            if responses.len() >= self.config.threshold {
                break;
            }
        }

        // loop only breaks when we have enough signatures
        tracing::info!(
            batch_number = batch_envelope.batch_number(),
            request_id = request_id,
            "Collected enough verification responses ({})",
            responses.len(),
        );

        // Cleanup: remove the channel for this request_id
        self.response_channels.write().await.remove(&request_id);

        Ok(responses)
    }

    /// Processes BatchVerificationResponse, on any error logs and returns None
    /// - extracts & validates signature
    /// - checks against list of accepted signers
    fn process_response(
        &self,
        commit_data: &CommitBatchInfo,
        request_id: u64,
        response: BatchVerificationResponse,
    ) -> Option<ValidatedBatchSignature> {
        let signature = match response {
            BatchVerificationResponse {
                result: BatchVerificationResult::Success(signature),
                ..
            } => signature,
            BatchVerificationResponse {
                result: BatchVerificationResult::Refused(reason),
                ..
            } => {
                tracing::info!(
                    batch_number = commit_data.batch_number,
                    request_id = request_id,
                    "Verification refused: {}",
                    reason
                );
                return None;
            }
        };

        let Ok(validated_signature) = signature.verify_signature(commit_data) else {
            tracing::warn!(
                batch_number = commit_data.batch_number,
                request_id = request_id,
                "Invalid signature",
            );
            return None;
        };

        if !self.accepted_signers.contains(validated_signature.signer()) {
            tracing::warn!(
                batch_number = commit_data.batch_number,
                request_id = request_id,
                signer = validated_signature.signer().to_string(),
                "Signature from unknown signer",
            );
            return None;
        }

        Some(validated_signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BatchVerificationResult;
    use crate::tests::{dummy_batch_envelope, dummy_commit_batch_info};
    use alloy::primitives::Address;
    use alloy::signers::local::PrivateKeySigner;
    use secrecy::SecretString;
    use tokio::sync::mpsc;
    use zksync_os_batch_types::{BatchSignature, ValidatedBatchSignature};
    use zksync_os_contract_interface::models::CommitBatchInfo;
    use zksync_os_l1_sender::batcher_model::{
        BatchForSigning, BatchSignatureData, SignedBatchEnvelope,
    };

    const DUMMY_ADDRESS: &str = "0x1111111111111111111111111111111111111111";

    fn test_config(accepted_signers: Vec<String>) -> BatchVerificationConfig {
        BatchVerificationConfig {
            server_enabled: true,
            listen_address: "127.0.0.1:0".to_string(),
            client_enabled: false,
            connect_address: String::new(),
            threshold: 1,
            accepted_signers,
            request_timeout: Duration::from_secs(5),
            retry_delay: Duration::from_millis(10),
            total_timeout: Duration::from_secs(10),
            // address 0x1DAeC5f53D365f4BBdA2d05Ed4FbE095b24AE15d
            signing_key: SecretString::new(
                "0xa4cabe6332985182371b02c0b117d9e83c8d608714b63f71fb000178ef25fa65".into(),
            ),
        }
    }

    async fn make_success_response(
        request_id: u64,
        commit_info: &CommitBatchInfo,
    ) -> (BatchVerificationResponse, Address) {
        let signer = PrivateKeySigner::random();
        let addr = signer.address();
        let sig = BatchSignature::sign_batch(commit_info, &signer).await;

        (
            BatchVerificationResponse {
                request_id,
                batch_number: commit_info.batch_number,
                result: BatchVerificationResult::Success(sig),
            },
            addr,
        )
    }

    fn make_verifier(accepted_signers: Vec<String>) -> BatchVerifier {
        let config = test_config(accepted_signers);
        let (server, _rx) = BatchVerificationServer::new();
        let server = Arc::new(server);
        let response_channels = Arc::new(RwLock::new(HashMap::new()));
        BatchVerifier::new(config, response_channels, server, 0)
    }

    #[tokio::test]
    async fn process_response_refused_returns_none() {
        let commit_info = dummy_commit_batch_info(1, 1, 2);
        let verifier = make_verifier(Vec::new());

        let response = BatchVerificationResponse {
            request_id: 1,
            batch_number: commit_info.batch_number,
            result: BatchVerificationResult::Refused("reason".to_string()),
        };

        let result = verifier.process_response(&commit_info, 1, response);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn process_response_unauthorized_signer_returns_none() {
        let commit_info = dummy_commit_batch_info(1, 1, 2);
        let (response, _addr) = make_success_response(1, &commit_info).await;

        let verifier = make_verifier(vec![DUMMY_ADDRESS.to_string()]);

        let result = verifier.process_response(&commit_info, 1, response);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn process_response_success_known_signer_returns_some() {
        let commit_info = dummy_commit_batch_info(1, 1, 2);
        let (response, addr) = make_success_response(1, &commit_info).await;
        let accepted = vec![DUMMY_ADDRESS.to_string(), addr.to_string()];
        let verifier = make_verifier(accepted);

        let result = verifier.process_response(&commit_info, 1, response);
        let validated: ValidatedBatchSignature =
            result.expect("expected Some(validated signature)");
        assert_eq!(validated.signer(), &addr);
    }

    #[tokio::test]
    async fn run_skips_already_committed_batches_and_forwards_them() {
        let accepted = Vec::new();
        let config = test_config(accepted);
        let (server, _rx_server) = BatchVerificationServer::new();
        let server = Arc::new(server);
        let response_channels = Arc::new(RwLock::new(HashMap::new()));
        let last_committed_batch_number = 10;
        let verifier = BatchVerifier::new(
            config,
            response_channels,
            server,
            last_committed_batch_number,
        );

        let (input_tx, input_rx) = mpsc::channel::<BatchForSigning<()>>(1);
        let (output_tx, mut output_rx) = mpsc::channel::<SignedBatchEnvelope<()>>(1);
        let peekable = zksync_os_pipeline::PeekableReceiver::new(input_rx);

        // batch with number 5 < 10, so it does not need to go through signing and should be forwarded as is
        let batch = dummy_batch_envelope(5, 30, 35);
        input_tx.send(batch).await.expect("failed to send batch");
        drop(input_tx);

        let run_handle = tokio::spawn(async move {
            verifier
                .run(peekable, output_tx)
                .await
                .expect("run should succeed");
        });

        let out = output_rx.recv().await.expect("expected output batch");
        match out.signature_data {
            BatchSignatureData::AlreadyCommitted => {}
            _ => panic!(
                "expected NotNeeded signature data, got: {:?}",
                out.signature_data
            ),
        }

        assert!(output_rx.recv().await.is_none());
        run_handle
            .await
            .expect("run task should complete, because input was closed");
    }

    #[tokio::test]
    async fn run_performs_signing_and_includes_signature() {
        // Prepare commit info and a valid signature from an accepted signer.
        let batch = dummy_batch_envelope(3, 10, 15);
        let commit_info = batch.batch.batch_info.commit_info.clone();
        let (response, addr) = make_success_response(1, &commit_info).await;
        let config = test_config(vec![addr.to_string()]);

        let (server, _rx_server) = BatchVerificationServer::new();
        // Ensure there is at least one subscriber so that send_verification_request
        // succeeds with threshold = 1 and to observe the outgoing request.
        let mut request_rx = server.subscribe_for_tests();
        let server = Arc::new(server);
        let response_channels = Arc::new(RwLock::new(HashMap::new()));
        let last_committed_batch_number = 0; // so no skipping - all batches need signing
        let verifier = BatchVerifier::new(
            config,
            response_channels.clone(),
            server,
            last_committed_batch_number,
        );

        // Spawn a helper task that waits for the outgoing verification request
        // via `request_rx`, then injects the prepared successful response for
        // the observed request_id.
        let response_channels_cloned = response_channels.clone();
        tokio::spawn(async move {
            let request = request_rx
                .recv()
                .await
                .expect("server should send a verification request");
            let request_id = request.request_id;

            let mut response = response;
            response.request_id = request_id;

            response_channels_cloned
                .read()
                .await
                .get(&request_id)
                .expect("sender should be available")
                .send(response)
                .await
                .expect("Failed to send");
        });

        // Wire up the pipeline: one input batch that must be signed, and an
        // output channel where we expect a signed batch.
        let (input_tx, input_rx) = mpsc::channel::<BatchForSigning<()>>(1);
        let (output_tx, mut output_rx) = mpsc::channel::<SignedBatchEnvelope<()>>(1);
        let peekable = zksync_os_pipeline::PeekableReceiver::new(input_rx);

        input_tx.send(batch).await.expect("failed to send batch");
        drop(input_tx);

        let run_handle = tokio::spawn(async move {
            verifier
                .run(peekable, output_tx)
                .await
                .expect("run should succeed");
        });

        let out = output_rx.recv().await.expect("expected output batch");
        match out.signature_data {
            BatchSignatureData::Signed { signatures } => {
                assert_eq!(signatures.len(), 1);
            }
            _ => panic!("expected Signed signature data"),
        }

        assert!(output_rx.recv().await.is_none());
        run_handle.await.expect("run task should complete");
    }
}
