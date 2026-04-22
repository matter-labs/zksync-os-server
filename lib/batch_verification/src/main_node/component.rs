use super::metrics::BATCH_VERIFICATION_SEQUENCER_METRICS;
use crate::config::BatchVerificationConfig;
use crate::verify_batch_wire::encode_verify_batch_request;
use alloy::primitives::Address;
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use zksync_os_batch_types::{BatchSignatureSet, ValidatedBatchSignature};
use zksync_os_contract_interface::l1_discovery::{BatchVerificationSL, L1State};
use zksync_os_l1_sender::batcher_metrics::BatchExecutionStage;
use zksync_os_l1_sender::batcher_model::{
    BatchForSigning, BatchSignatureData, SignedBatchEnvelope,
};
use zksync_os_network::{PeerVerifyBatchResult, VerifyBatch, VerifyBatchOutcome};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState};
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};

pub struct BatchVerificationPipelineStep<E> {
    config: BatchVerificationConfig,
    threshold: u64,
    validators: Vec<Address>,
    last_committed_batch_number: u64,
    l1_state: L1State,
    verify_request_tx: mpsc::Sender<VerifyBatch>,
    verify_result_rx: mpsc::Receiver<PeerVerifyBatchResult>,
    _phantom: std::marker::PhantomData<E>,
}

impl<E> BatchVerificationPipelineStep<E> {
    pub fn new(
        config: BatchVerificationConfig,
        l1_state: L1State,
        last_committed_batch_number: u64,
        verify_request_tx: mpsc::Sender<VerifyBatch>,
        verify_result_rx: mpsc::Receiver<PeerVerifyBatchResult>,
    ) -> Self {
        let (threshold, validators) = effective_verification_policy(&config, &l1_state);

        Self {
            config,
            threshold,
            validators,
            last_committed_batch_number,
            l1_state,
            verify_request_tx,
            verify_result_rx,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Returns the effective batch-verification threshold and validator set after merging local
/// server config with the current L1 policy.
pub fn effective_verification_policy(
    config: &BatchVerificationConfig,
    l1_state: &L1State,
) -> (u64, Vec<Address>) {
    let config_validators = config
        .accepted_signers
        .clone()
        .into_iter()
        .map(|signer| signer.parse().unwrap())
        .collect();

    match &l1_state.batch_verification {
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
        tracing::info!(
            enabled = self.config.server_enabled,
            threshold = self.threshold,
            "starting batch verification pipeline step"
        );
        if !self.config.server_enabled {
            while let Some(batch) = input.recv().await {
                output
                    .send(batch.with_signatures(BatchSignatureData::NotNeeded))
                    .await
                    .map_err(|_| anyhow::anyhow!("Failed to send signed batch envelope"))?;
            }
            return Ok(());
        }

        let verifier = BatchVerificationRunner::new(self);
        verifier.run(input, output).await
    }
}

struct BatchVerificationRunner {
    config: BatchVerificationConfig,
    accepted_signers: Vec<Address>,
    threshold: u64,
    request_id_counter: AtomicU64,
    verify_request_tx: mpsc::Sender<VerifyBatch>,
    verify_result_rx: mpsc::Receiver<PeerVerifyBatchResult>,
    l1_chain_id: u64,
    multisig_committer: Address,
    last_committed_batch_number: u64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BatchVerificationError {
    #[error("Not enough signers: {0} < {1}")]
    NotEnoughSigners(u64, u64),
    #[error("Verify request channel closed")]
    VerifyRequestChannelClosed,
    #[error("Internal error: {0}")]
    Internal(String),
}

impl BatchVerificationError {
    fn retryable(&self) -> bool {
        matches!(self, BatchVerificationError::NotEnoughSigners(..))
    }
}

impl BatchVerificationRunner {
    fn new<E>(component: BatchVerificationPipelineStep<E>) -> Self {
        BATCH_VERIFICATION_SEQUENCER_METRICS
            .threshold
            .set(component.threshold);
        BATCH_VERIFICATION_SEQUENCER_METRICS
            .validators_count
            .set(component.validators.len());

        Self {
            config: component.config,
            accepted_signers: component.validators,
            threshold: component.threshold,
            request_id_counter: AtomicU64::new(1),
            verify_request_tx: component.verify_request_tx,
            verify_result_rx: component.verify_result_rx,
            l1_chain_id: component.l1_state.sl_chain_id,
            multisig_committer: component.l1_state.validator_timelock_sl,
            last_committed_batch_number: component.last_committed_batch_number,
        }
    }

    async fn run<E: Send + Sync>(
        mut self,
        mut batch_for_signing_receiver: PeekableReceiver<BatchForSigning<E>>,
        signed_batch_sender: mpsc::Sender<SignedBatchEnvelope<E>>,
    ) -> anyhow::Result<()> {
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "batch_verification_runner",
            GenericComponentState::WaitingRecv,
        );
        let metrics = &*BATCH_VERIFICATION_SEQUENCER_METRICS;

        'runner: loop {
            latency_tracker.enter_state(GenericComponentState::WaitingRecv);
            let Some(batch_envelope) = batch_for_signing_receiver.recv().await else {
                tracing::info!("BatchForSigning channel closed, exiting batch verification runner");
                break Ok(());
            };
            tracing::info!(
                batch_number = batch_envelope.batch_number(),
                "received batch for verification"
            );

            if batch_envelope.batch_number() <= self.last_committed_batch_number {
                tracing::info!(
                    "Skipping signing of already committed batch {}",
                    batch_envelope.batch_number()
                );
                signed_batch_sender
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
                    Err(BatchVerificationError::VerifyRequestChannelClosed) => {
                        tracing::info!(
                            batch_number = batch_envelope.batch_number(),
                            "Verify request channel closed, exiting batch verification runner"
                        );
                        break 'runner Ok(());
                    }
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
                    Err(err) => break Err(err),
                }
            }?;

            metrics.attempts_to_success.observe(retry_count + 1);
            metrics.total_latency.observe(start_time.elapsed());

            latency_tracker.enter_state(GenericComponentState::WaitingSend);
            signed_batch_sender
                .send(
                    batch_envelope
                        .with_signatures(BatchSignatureData::Signed { signatures })
                        .with_stage(BatchExecutionStage::BatchSigned),
                )
                .await
                .map_err(|_| anyhow::anyhow!("Failed to send signed batch envelope"))?;
        }
    }

    async fn collect_batch_verification_signatures<E: Send + Sync>(
        &mut self,
        batch_envelope: &BatchForSigning<E>,
        attempt_number: u64,
    ) -> Result<BatchSignatureSet, BatchVerificationError> {
        let metrics = &*BATCH_VERIFICATION_SEQUENCER_METRICS;
        let request_id = self.request_id_counter.fetch_add(1, Ordering::SeqCst);
        metrics.last_request_id.set(request_id);

        let request = encode_verify_batch_request(batch_envelope, request_id)?;
        tracing::info!(
            batch_number = batch_envelope.batch_number(),
            request_id,
            "Starting batch verification"
        );
        self.verify_request_tx
            .send(request)
            .await
            .map_err(|_| BatchVerificationError::VerifyRequestChannelClosed)?;

        let mut responses = BatchSignatureSet::new();
        let start_time = Instant::now();
        let deadline = Instant::now() + self.config.request_timeout;

        loop {
            let remaining_time = deadline - Instant::now();
            if remaining_time <= Duration::from_secs(0) {
                let responses_len = u64::try_from(responses.len()).unwrap();
                return Err(BatchVerificationError::NotEnoughSigners(
                    responses_len,
                    self.threshold,
                ));
            }

            let response =
                match tokio::time::timeout(remaining_time, self.verify_result_rx.recv()).await {
                    Ok(Some(response)) => response,
                    Ok(None) => {
                        return Err(BatchVerificationError::Internal(
                            "Verify result channel closed".to_string(),
                        ));
                    }
                    Err(_) => {
                        let responses_len = u64::try_from(responses.len()).unwrap();
                        return Err(BatchVerificationError::NotEnoughSigners(
                            responses_len,
                            self.threshold,
                        ));
                    }
                };

            if response.message.request_id != request_id {
                tracing::debug!(
                    request_id,
                    received_request_id = response.message.request_id,
                    "ignoring verify result for different request"
                );
                continue;
            }

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
                    request_id,
                    signer = signer,
                    "Received duplicated signature",
                );
                continue;
            }

            tracing::debug!(
                batch_number = batch_envelope.batch_number(),
                request_id,
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

        tracing::info!(
            batch_number = batch_envelope.batch_number(),
            request_id,
            "Collected enough verification responses ({})",
            responses.len(),
        );

        Ok(responses)
    }

    fn process_response<E>(
        &self,
        batch_envelope: &BatchForSigning<E>,
        request_id: u64,
        response: PeerVerifyBatchResult,
    ) -> Option<ValidatedBatchSignature> {
        let signature = match response.message.result {
            VerifyBatchOutcome::Approved(signature) => {
                let Ok(signature) = <[u8; 65]>::try_from(signature.as_ref()) else {
                    BATCH_VERIFICATION_SEQUENCER_METRICS.failed_responses[&"invalid_signature"]
                        .inc();
                    tracing::warn!(
                        request_id,
                        batch_number = batch_envelope.batch_number(),
                        "Malformed signature length"
                    );
                    return None;
                };
                match zksync_os_batch_types::BatchSignature::from_raw_array(&signature) {
                    Ok(signature) => signature,
                    Err(err) => {
                        BATCH_VERIFICATION_SEQUENCER_METRICS.failed_responses[&"invalid_signature"]
                            .inc();
                        tracing::warn!(%err, request_id, batch_number = batch_envelope.batch_number(), "Malformed signature");
                        return None;
                    }
                }
            }
            VerifyBatchOutcome::Refused(reason) => {
                BATCH_VERIFICATION_SEQUENCER_METRICS.failed_responses[&"refused"].inc();
                tracing::info!(
                    peer_id = %response.peer_id,
                    batch_number = batch_envelope.batch_number(),
                    request_id,
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
            peer_id = %response.peer_id,
            batch_number = batch_envelope.batch_number(),
            request_id,
                "Invalid signature",
            );
            return None;
        };

        if !self.accepted_signers.contains(validated_signature.signer()) {
            BATCH_VERIFICATION_SEQUENCER_METRICS.failed_responses[&"unknown_signer"].inc();
            tracing::warn!(
                peer_id = %response.peer_id,
                batch_number = batch_envelope.batch_number(),
                request_id,
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
    use crate::tests::dummy_batch_envelope;
    use alloy::primitives::{Address, b512};
    use alloy::signers::local::PrivateKeySigner;
    use secrecy::SecretString;
    use tokio::sync::mpsc;
    use zksync_os_batch_types::{BatchSignature, ValidatedBatchSignature};
    use zksync_os_l1_sender::batcher_model::{
        BatchForSigning, BatchSignatureData, SignedBatchEnvelope,
    };
    use zksync_os_network::{PeerVerifyBatchResult, VerifyBatchResult};

    const DUMMY_ADDRESS: &str = "0x1111111111111111111111111111111111111111";
    const CHAIN_ID: u64 = 1;
    const MULTISIG_COMMITTER_DUMMY: &str = "0x2222222222222222222222222222222222222222";

    fn test_config(accepted_signers: Vec<String>) -> BatchVerificationConfig {
        BatchVerificationConfig {
            server_enabled: true,
            client_enabled: false,
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

    fn dummy_peer_response(result: VerifyBatchResult) -> PeerVerifyBatchResult {
        PeerVerifyBatchResult {
            peer_id: b512!(
                "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001"
            ),
            message: result,
        }
    }

    async fn make_success_response<E>(
        request_id: u64,
        batch: &BatchForSigning<E>,
    ) -> (PeerVerifyBatchResult, Address) {
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
            dummy_peer_response(VerifyBatchResult {
                request_id,
                batch_number: batch.batch_number(),
                result: VerifyBatchOutcome::Approved(sig.into_raw().to_vec().into()),
            }),
            addr,
        )
    }

    fn make_verifier(
        accepted_signers: Vec<String>,
        last_committed_batch_number: u64,
    ) -> (
        BatchVerificationRunner,
        mpsc::Receiver<VerifyBatch>,
        mpsc::Sender<PeerVerifyBatchResult>,
    ) {
        let config = test_config(accepted_signers.clone());
        let (verify_request_tx, verify_request_rx) = mpsc::channel(1);
        let (verify_result_tx, verify_result_rx) = mpsc::channel(1);
        let accepted_signers_addrs: Vec<Address> = accepted_signers
            .into_iter()
            .map(|signer| signer.parse().unwrap())
            .collect();
        let threshold = config.threshold;
        let verifier = BatchVerificationRunner {
            config,
            accepted_signers: accepted_signers_addrs,
            threshold,
            request_id_counter: AtomicU64::new(1),
            verify_request_tx,
            verify_result_rx,
            l1_chain_id: CHAIN_ID,
            multisig_committer: MULTISIG_COMMITTER_DUMMY.parse().unwrap(),
            last_committed_batch_number,
        };
        (verifier, verify_request_rx, verify_result_tx)
    }

    #[tokio::test]
    async fn process_response_refused_returns_none() {
        let batch = dummy_batch_envelope(1, 1, 2);
        let (verifier, _verify_request_rx, _verify_result_tx) = make_verifier(Vec::new(), 0);

        let response = dummy_peer_response(VerifyBatchResult {
            request_id: 1,
            batch_number: batch.batch_number(),
            result: VerifyBatchOutcome::Refused("reason".to_string()),
        });

        let result = verifier.process_response(&batch, 1, response);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn process_response_unauthorized_signer_returns_none() {
        let batch = dummy_batch_envelope(1, 1, 2);
        let (response, _addr) = make_success_response(1, &batch).await;

        let (verifier, _verify_request_rx, _verify_result_tx) =
            make_verifier(vec![DUMMY_ADDRESS.to_string()], 0);

        let result = verifier.process_response(&batch, 1, response);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn process_response_success_known_signer_returns_some() {
        let batch = dummy_batch_envelope(1, 1, 2);
        let (response, addr) = make_success_response(1, &batch).await;
        let accepted = vec![DUMMY_ADDRESS.to_string(), addr.to_string()];
        let (verifier, _verify_request_rx, _verify_result_tx) = make_verifier(accepted, 0);

        let result = verifier.process_response(&batch, 1, response);
        let validated: ValidatedBatchSignature =
            result.expect("expected Some(validated signature)");
        assert_eq!(validated.signer(), &addr);
    }

    #[tokio::test]
    async fn run_skips_already_committed_batches_and_forwards_them() {
        let (verifier, _verify_request_rx, _verify_result_tx) = make_verifier(Vec::new(), 10);

        let (input_tx, input_rx) = mpsc::channel::<BatchForSigning<()>>(1);
        let (output_tx, mut output_rx) = mpsc::channel::<SignedBatchEnvelope<()>>(1);
        let peekable = zksync_os_pipeline::PeekableReceiver::new(input_rx);

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
                "expected AlreadyCommitted signature data, got: {:?}",
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
        let batch = dummy_batch_envelope(3, 10, 15);
        let (response, addr) = make_success_response(1, &batch).await;
        let (verifier, mut verify_request_rx, verify_result_tx) =
            make_verifier(vec![addr.to_string()], 0);

        tokio::spawn(async move {
            let request = verify_request_rx
                .recv()
                .await
                .expect("verifier should send a verification request");
            let mut response = response;
            response.message.request_id = request.request_id;
            verify_result_tx
                .send(response)
                .await
                .expect("failed to send verification response");
        });

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

    #[tokio::test]
    async fn run_returns_ok_if_verify_request_channel_is_closed() {
        let batch = dummy_batch_envelope(3, 10, 15);
        let (verifier, verify_request_rx, _verify_result_tx) = make_verifier(Vec::new(), 0);
        drop(verify_request_rx);

        let (input_tx, input_rx) = mpsc::channel::<BatchForSigning<()>>(1);
        let (output_tx, mut output_rx) = mpsc::channel::<SignedBatchEnvelope<()>>(1);
        let peekable = zksync_os_pipeline::PeekableReceiver::new(input_rx);

        input_tx.send(batch).await.expect("failed to send batch");
        drop(input_tx);

        let run_handle = tokio::spawn(async move { verifier.run(peekable, output_tx).await });

        run_handle
            .await
            .expect("run task should complete")
            .expect("run should exit successfully when verify request channel is closed");
        assert!(output_rx.recv().await.is_none());
    }
}
