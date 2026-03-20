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
use zksync_os_contract_interface::l1_discovery::{BatchVerificationSL, L1State};
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{
    BatchForSigning, BatchSignatureData, SignedBatchEnvelope,
};
use zksync_os_observability::{ComponentHealthReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

fn report_exit<T, E: std::fmt::Debug>(name: &'static str) -> impl Fn(Result<T, E>) {
    move |result| match result {
        Ok(_) => tracing::warn!("{name} unexpectedly exited"),
        Err(e) => tracing::error!("{name} failed: {e:#?}"),
    }
}
pub struct BatchVerificationPipelineStep<E> {
    config: BatchVerificationConfig,
    threshold: u64,
    validators: Vec<Address>,
    last_committed_batch_number: u64,
    l1_state: L1State,
    pub health_reporter: ComponentHealthReporter,
    _phantom: std::marker::PhantomData<E>,
}

impl<E> BatchVerificationPipelineStep<E> {
    pub fn new(
        config: BatchVerificationConfig,
        l1_state: L1State,
        last_committed_batch_number: u64,
        health_reporter: ComponentHealthReporter,
    ) -> Self {
        let config_validators = config
            .accepted_signers
            .clone()
            .into_iter()
            .map(|s| s.parse().unwrap())
            .collect();
        // If on L1 batch verifiers re configured, we use that configuration instead
        let (threshold, validators) = match &l1_state.batch_verification {
            BatchVerificationSL::Enabled(l1_config) => {
                if !l1_config.validators.is_empty() || l1_config.threshold > 0 {
                    (
                        config.threshold.max(l1_config.threshold),
                        l1_config.validators.clone(),
                    )
                } else {
                    (config.threshold, config_validators)
                }
            }
            BatchVerificationSL::Disabled => (config.threshold, config_validators),
        };

        Self {
            config,
            threshold,
            validators,
            last_committed_batch_number,
            l1_state,
            health_reporter,
            _phantom: std::marker::PhantomData,
        }
    }
}

type ResponseChannelsMapArc = Arc<RwLock<HashMap<u64, Sender<BatchVerificationResponse>>>>;

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

            // Destructure to get health_reporter by value while passing &self to BatchVerifier::new
            // (avoiding partial move conflict by using struct literal construction)
            let BatchVerificationPipelineStep {
                config,
                threshold,
                validators,
                last_committed_batch_number,
                l1_state,
                health_reporter,
                _phantom,
            } = self;

            BATCH_VERIFICATION_SEQUENCER_METRICS
                .threshold
                .set(threshold);
            BATCH_VERIFICATION_SEQUENCER_METRICS
                .validators_count
                .set(validators.len());

            let verifier = BatchVerifier {
                config,
                accepted_signers: validators,
                threshold,
                request_id_counter: AtomicU64::new(1),
                response_channels,
                server,
                l1_chain_id: l1_state.sl_chain_id,
                multisig_committer: l1_state.validator_timelock_sl,
                last_committed_batch_number,
                health_reporter,
            };
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
    response_channels: ResponseChannelsMapArc,
) -> anyhow::Result<()> {
    let (health_reporter, _rx) = ComponentHealthReporter::new("batch_response_processor");
    while let Some(response) = response_receiver.recv().await {
        health_reporter.enter_state(GenericComponentState::Processing);
        let request_id = response.request_id;

        // Route response to the appropriate channel
        if let Some(sender) = response_channels.read().await.get(&request_id) {
            tracing::debug!(request_id, "Received batch verification response");
            health_reporter.enter_state(GenericComponentState::WaitingSend);
            if let Err(e) = sender.send(response).await {
                tracing::warn!(request_id, ?e, "Failed to route response");
            }
        } else {
            // debug, because probably we finished processing this batch and this is an extra response
            tracing::debug!(request_id, "Response for unknown request_id, dropping");
        }
        health_reporter.enter_state(GenericComponentState::WaitingRecv);
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
    threshold: u64,
    request_id_counter: AtomicU64,
    server: Arc<BatchVerificationServer>,
    response_channels: ResponseChannelsMapArc,
    l1_chain_id: u64,
    multisig_committer: Address,
    last_committed_batch_number: u64,
    health_reporter: ComponentHealthReporter,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Timeout")]
    Timeout,
    #[error("Not enough signers: {0} < {1}")]
    NotEnoughSigners(u64, u64),
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
    async fn run<E: Send + Sync>(
        &self,
        mut batch_for_signing_receiver: PeekableReceiver<BatchForSigning<E>>,
        singed_batcher_sender: Sender<SignedBatchEnvelope<E>>,
    ) -> anyhow::Result<()> {
        let health_reporter = &self.health_reporter;
        let metrics = &*BATCH_VERIFICATION_SEQUENCER_METRICS;

        loop {
            health_reporter.enter_state(GenericComponentState::WaitingRecv);
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

            health_reporter.enter_state(GenericComponentState::Processing);
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

            let last_block = batch_envelope.batch.last_block_number;
            health_reporter.enter_state(GenericComponentState::WaitingSend);
            singed_batcher_sender
                .send(
                    batch_envelope
                        .with_signatures(BatchSignatureData::Signed { signatures })
                        .with_stage(BatchExecutionStage::BatchSigned),
                )
                .await
                .map_err(|_| anyhow::anyhow!("Failed to send signed batch envelope"))?;
            health_reporter.record_processed(last_block);
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
            mpsc::channel::<BatchVerificationResponse>(self.threshold.try_into().unwrap());

        // Register the channel for this request_id
        self.response_channels
            .write()
            .await
            .insert(request_id, response_sender);

        // Send verification request to all connected clients
        self.server
            .send_verification_request(batch_envelope, request_id, self.threshold)
            .await?;

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
                self.process_response(batch_envelope, request_id, response)
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
                self.threshold
            );

            if u64::try_from(responses.len()).unwrap() >= self.threshold {
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
    fn process_response<E>(
        &self,
        batch_envelope: &BatchForSigning<E>,
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
                BATCH_VERIFICATION_SEQUENCER_METRICS.failed_responses[&"refused"].inc();
                tracing::info!(
                    batch_number = batch_envelope.batch_number(),
                    request_id = request_id,
                    "Verification refused: {}",
                    reason
                );
                return None;
            }
        };

        let Ok(validated_signature) = signature.verify_signature(
            &batch_envelope.batch.previous_stored_batch_info,
            &batch_envelope.batch.batch_info,
            self.l1_chain_id,
            self.multisig_committer,
            &batch_envelope.batch.protocol_version,
        ) else {
            BATCH_VERIFICATION_SEQUENCER_METRICS.failed_responses[&"invalid_signature"].inc();
            tracing::warn!(
                batch_number = batch_envelope.batch_number(),
                request_id = request_id,
                "Invalid signature",
            );
            return None;
        };

        if !self.accepted_signers.contains(validated_signature.signer()) {
            BATCH_VERIFICATION_SEQUENCER_METRICS.failed_responses[&"unknown_signer"].inc();
            tracing::warn!(
                batch_number = batch_envelope.batch_number(),
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
    use crate::tests::dummy_batch_envelope;
    use alloy::primitives::Address;
    use alloy::signers::local::PrivateKeySigner;
    use secrecy::SecretString;
    use tokio::sync::mpsc;
    use zksync_os_batch_types::{BatchSignature, ValidatedBatchSignature};
    use zksync_os_l1_sender::batcher_model::{
        BatchForSigning, BatchSignatureData, SignedBatchEnvelope,
    };

    const DUMMY_ADDRESS: &str = "0x1111111111111111111111111111111111111111";
    const CHAIN_ID: u64 = 1;
    const MULTISIG_COMMITTER_DUMMY: &str = "0x2222222222222222222222222222222222222222";

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

    async fn make_success_response<E>(
        request_id: u64,
        batch: &BatchForSigning<E>,
    ) -> (BatchVerificationResponse, Address) {
        let signer = PrivateKeySigner::random();
        let addr = signer.address();
        let sig = BatchSignature::sign_batch(
            &batch.batch.previous_stored_batch_info,
            &batch.batch.batch_info,
            CHAIN_ID,
            MULTISIG_COMMITTER_DUMMY.parse().unwrap(),
            &batch.batch.protocol_version,
            &signer,
        )
        .await;

        (
            BatchVerificationResponse {
                request_id,
                batch_number: batch.batch_number(),
                result: BatchVerificationResult::Success(sig),
            },
            addr,
        )
    }

    fn make_verifier(
        accepted_signers: Vec<String>,
        last_committed_batch_number: u64,
    ) -> (BatchVerifier, ResponseChannelsMapArc) {
        let config = test_config(accepted_signers.clone());
        let (server, _rx) = BatchVerificationServer::new();
        let server = Arc::new(server);
        let response_channels = Arc::new(RwLock::new(HashMap::new()));
        let accepted_signers_addrs: Vec<Address> = accepted_signers
            .into_iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let threshold = config.threshold;
        let (health_reporter, _rx) = ComponentHealthReporter::new("batch_verifier");
        let verifier = BatchVerifier {
            config,
            accepted_signers: accepted_signers_addrs,
            threshold,
            response_channels: response_channels.clone(),
            server,
            l1_chain_id: CHAIN_ID,
            multisig_committer: MULTISIG_COMMITTER_DUMMY.parse().unwrap(),
            last_committed_batch_number,
            request_id_counter: AtomicU64::new(1),
            health_reporter,
        };
        (verifier, response_channels)
    }

    #[tokio::test]
    async fn process_response_refused_returns_none() {
        let batch = dummy_batch_envelope(1, 1, 2);
        let (verifier, _) = make_verifier(Vec::new(), 0);

        let response = BatchVerificationResponse {
            request_id: 1,
            batch_number: batch.batch_number(),
            result: BatchVerificationResult::Refused("reason".to_string()),
        };

        let result = verifier.process_response(&batch, 1, response);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn process_response_unauthorized_signer_returns_none() {
        let batch = dummy_batch_envelope(1, 1, 2);
        let (response, _addr) = make_success_response(1, &batch).await;

        let (verifier, _) = make_verifier(vec![DUMMY_ADDRESS.to_string()], 0);

        let result = verifier.process_response(&batch, 1, response);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn process_response_success_known_signer_returns_some() {
        let batch = dummy_batch_envelope(1, 1, 2);
        let (response, addr) = make_success_response(1, &batch).await;
        let accepted = vec![DUMMY_ADDRESS.to_string(), addr.to_string()];
        let (verifier, _) = make_verifier(accepted, 0);

        let result = verifier.process_response(&batch, 1, response);
        let validated: ValidatedBatchSignature =
            result.expect("expected Some(validated signature)");
        assert_eq!(validated.signer(), &addr);
    }

    #[tokio::test]
    async fn run_skips_already_committed_batches_and_forwards_them() {
        let accepted = Vec::new();
        let (verifier, _) = make_verifier(accepted, 10);

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
        let (response, addr) = make_success_response(1, &batch).await;
        let (verifier, response_channels) = make_verifier(vec![addr.to_string()], 0);

        // Ensure there is at least one subscriber so that send_verification_request
        // succeeds with threshold = 1 and to observe the outgoing request.
        let mut request_rx = verifier.server.subscribe_for_tests();

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
