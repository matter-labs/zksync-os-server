use crate::client::metrics::BATCH_VERIFICATION_CLIENT_METRICS;
use crate::{
    BatchVerificationRequest, BatchVerificationRequestDecoder, BatchVerificationResponse,
    BatchVerificationResponseCodec, BatchVerificationResult,
    wire_format::ensure_supported_wire_format,
};
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use anyhow::anyhow;
use async_trait::async_trait;
use block_cache::BlockCache;
use futures::{SinkExt, StreamExt, TryStreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use secrecy::{ExposeSecret, SecretString};
use std::io;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};
use std::time::Duration;
use structdiff::StructDiff;
use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::io::StreamReader;
use tokio_util::sync::PollSender;
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_batch_types::{BatchInfo, BatchSignature};
use zksync_os_contract_interface::l1_discovery::{BatchVerificationSL, L1State};
use zksync_os_interface::types::BlockOutput;
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_observability::ComponentStateHandle;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_observability::GenericComponentState;
use zksync_os_observability::StateLabel;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::{ReadFinality, ReadStateHistory};
use zksync_os_storage_api::{ReplayRecord, StateError, read_multichain_root};

mod block_cache;
mod metrics;

/// Client that connects to the main sequencer for batch verification
pub struct BatchVerificationClient<Finality, ReadState> {
    chain_id: u64,
    diamond_proxy_sl: Address,
    server_address: String,
    l1_state: L1State,
    signer: PrivateKeySigner,
    block_cache: BlockCache<Finality, (BlockOutput, ReplayRecord, BlockMerkleTreeData)>,
    read_state: ReadState,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Missing records for block {0}")]
    MissingBlock(u64),
    #[error("Tree error")]
    TreeError,
    #[error("Batch data mismatch: {0}")]
    BatchDataMismatch(String),
    #[error("State error: {0}")]
    State(#[from] StateError),
}

type VerificationInput = (BlockOutput, ReplayRecord, BlockMerkleTreeData);

impl<Finality: ReadFinality, ReadState: ReadStateHistory>
    BatchVerificationClient<Finality, ReadState>
{
    pub fn new(
        chain_id: u64,
        diamond_proxy_sl: Address,
        server_address: String,
        private_key: SecretString,
        finality: Finality,
        l1_state: L1State,
        read_state: ReadState,
    ) -> Self {
        let signer = PrivateKeySigner::from_str(private_key.expose_secret())
            .expect("Invalid batch verification private key");
        if let BatchVerificationSL::Enabled(l1_config) = l1_state.batch_verification.clone()
            && !l1_config.validators.contains(&signer.address())
        {
            tracing::warn!(
                address = %signer.address(),
                "Your address is not authorized to verify batches on L1",
            );
        }

        Self {
            chain_id,
            diamond_proxy_sl,
            server_address,
            l1_state,
            signer,
            block_cache: BlockCache::new(finality),
            read_state,
        }
    }

    async fn connect_and_handle(
        &mut self,
        input: &mut PeekableReceiver<VerificationInput>,
        latency_tracker: &ComponentStateHandle<BatchVerificationClientState>,
    ) -> anyhow::Result<()> {
        // Create channel for sending request data
        let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, io::Error>>(128);

        // Convert channel receiver to a body stream
        let request_body =
            StreamBody::new(ReceiverStream::new(rx).map(|r| r.map_err(io::Error::other)));

        let req = hyper::Request::builder()
            .method("POST")
            .uri(format!("{}/batch_verification", self.server_address))
            .header("content-type", "application/octet-stream")
            .body(request_body)?;

        // Build HTTPS connector
        let https = HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(rustls::crypto::ring::default_provider())?
            .https_or_http() // Support both HTTPS and HTTP
            .enable_http2()
            .build();

        let client = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build(https);

        // Send request and get response future (doesn't block on body completion)
        let response_future = client.request(req);

        // Get response (will have headers, body streams separately)
        let response = response_future.await?;

        if !response.status().is_success() {
            let body_bytes = response.collect().await?.to_bytes();
            let text = String::from_utf8_lossy(&body_bytes);
            return Err(anyhow!("request failed: {}", text));
        }

        let stream = response.into_body().into_data_stream();
        let stream = stream.map_err(io::Error::other);

        let mut reader = StreamReader::new(stream);
        let batch_verification_version = reader.read_u32().await?;
        ensure_supported_wire_format(batch_verification_version)?;
        let mut reader = FramedRead::new(reader, BatchVerificationRequestDecoder::new());
        let mut writer = FramedWrite::new(
            ChannelWriter::new(tx),
            BatchVerificationResponseCodec::new(),
        );

        let address = self.signer.address().to_string();
        tracing::info!(
            address,
            "Connected to main sequencer for batch verification",
        );

        loop {
            latency_tracker.enter_state(BatchVerificationClientState::WaitingRecv);
            tokio::select! {
                block = input.recv() => {
                    match block {
                        Some((block_output, replay_record, tree_data)) => {
                            // we remove blocks from cache based on incoming signing requests.
                            // this prevent memory exhaustion / leak
                            self.block_cache.insert(
                                replay_record.block_context.block_number,
                                (block_output, replay_record, tree_data),
                            )?;
                        }
                        None => return Ok(()), // Channel closed, we are stopping now
                    }
                }
                // Handling in sequence without concurrency is fine as we shouldn't get too many requests and they should handle fast
                server_message = reader.next() => {
                    match server_message {
                        Some(Ok(message)) => {
                            latency_tracker.enter_state(BatchVerificationClientState::Processing);

                            let batch_number = message.batch_number;
                            let request_id = message.request_id;
                            let verification_result = self.handle_verification_request(message).await;

                            latency_tracker.enter_state(BatchVerificationClientState::WaitingSend);
                            match verification_result {
                                Ok(signature) => {
                                    tracing::info!(batch_number, request_id, address, "Approved batch verification request");
                                    BATCH_VERIFICATION_CLIENT_METRICS.record_request_success(request_id, batch_number);
                                    writer.send(BatchVerificationResponse { request_id, batch_number, result: BatchVerificationResult::Success(signature) }).await?;
                                },
                                Err(reason) => {
                                    tracing::info!(batch_number, request_id, address, "Batch verification failed: {}", reason);
                                    BATCH_VERIFICATION_CLIENT_METRICS.record_request_failure(request_id, batch_number);
                                    writer.send(BatchVerificationResponse { request_id, batch_number, result: BatchVerificationResult::Refused(reason.to_string()) }).await?;
                                },
                            }
                        }
                        Some(Err(parsing_err)) =>
                        {
                            tracing::warn!("Error parsing verification request message. Ignoring: {}", parsing_err);
                        }
                        None => {
                            anyhow::bail!("Server has disconnected verification client");
                        }
                    }
                }
            }
        }
    }

    async fn handle_verification_request(
        &self,
        request: BatchVerificationRequest,
    ) -> Result<BatchSignature, BatchVerificationError> {
        tracing::info!(
            batch_number = request.batch_number,
            request_id = request.request_id,
            "Handling batch verification request (blocks {}-{})",
            request.first_block_number,
            request.last_block_number,
        );

        let blocks: Vec<(&BlockOutput, &ReplayRecord, TreeBatchOutput)> =
            (request.first_block_number..=request.last_block_number)
                .map(|block_number| {
                    let (block_output, replay_record, tree_data) = self
                        .block_cache
                        .get(block_number)
                        .ok_or(BatchVerificationError::MissingBlock(block_number))?;

                    let (root_hash, leaf_count) = tree_data
                        .block_end
                        .clone()
                        .root_info()
                        .map_err(|_| BatchVerificationError::TreeError)?;

                    let tree_output = TreeBatchOutput {
                        root_hash,
                        leaf_count,
                    };
                    Ok((block_output, replay_record, tree_output))
                })
                .collect::<Result<Vec<_>, BatchVerificationError>>()?;

        let state_view = self.read_state.state_view_at(request.last_block_number)?;
        let multichain_root = read_multichain_root(state_view);

        let batch_info = BatchInfo::new(
            blocks
                .iter()
                .map(|(block_output, replay_record, tree)| {
                    (
                        *block_output,
                        &replay_record.block_context,
                        replay_record.transactions.as_slice(),
                        tree,
                    )
                })
                .collect(),
            self.chain_id,
            self.diamond_proxy_sl,
            request.batch_number,
            request.pubdata_mode,
            self.l1_state.sl_chain_id,
            multichain_root,
            &blocks.first().unwrap().1.protocol_version,
        );

        let expected_commit_data = batch_info.commit_info.clone().into();
        if expected_commit_data != request.commit_data {
            let diff = request.commit_data.diff(&expected_commit_data);

            return Err(BatchVerificationError::BatchDataMismatch(format!(
                "Batch data mismatch: {diff:?}",
            )));
        }

        let signature = BatchSignature::sign_batch(
            &request.prev_commit_data,
            &batch_info,
            self.l1_state.sl_chain_id,
            self.l1_state.validator_timelock_sl,
            &blocks.first().unwrap().1.protocol_version,
            &self.signer,
        )
        .await;

        Ok(signature)
    }
}

enum BatchVerificationClientState {
    Connecting,
    WaitingRecv,
    Processing,
    WaitingSend,
}

impl StateLabel for BatchVerificationClientState {
    fn generic(&self) -> GenericComponentState {
        match self {
            BatchVerificationClientState::Connecting => GenericComponentState::WaitingRecv,
            BatchVerificationClientState::WaitingRecv => GenericComponentState::WaitingRecv,
            BatchVerificationClientState::Processing => GenericComponentState::Processing,
            BatchVerificationClientState::WaitingSend => GenericComponentState::WaitingSend,
        }
    }

    fn specific(&self) -> &'static str {
        match self {
            BatchVerificationClientState::Connecting => "connecting",
            BatchVerificationClientState::WaitingRecv => {
                GenericComponentState::WaitingRecv.specific()
            }
            BatchVerificationClientState::Processing => {
                GenericComponentState::Processing.specific()
            }
            BatchVerificationClientState::WaitingSend => {
                GenericComponentState::WaitingSend.specific()
            }
        }
    }
}

#[async_trait]
impl<Finality: ReadFinality, ReadState: ReadStateHistory> PipelineComponent
    for BatchVerificationClient<Finality, ReadState>
{
    type Input = VerificationInput;
    type Output = ();

    const NAME: &'static str = "batch_verification_client";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        // Did not use backon due to borrowing issues
        let latency_tracker = ComponentStateReporter::global().handle_for(
            "batch_verification_client",
            BatchVerificationClientState::Connecting,
        );
        loop {
            let result = self.connect_and_handle(&mut input, &latency_tracker).await;

            match result {
                Ok(()) => {
                    // Normal shutdown - input channel closed
                    return Ok(());
                }
                Err(err) => {
                    latency_tracker.enter_state(BatchVerificationClientState::Connecting);
                    tracing::info!(
                        ?err,
                        "Connection to batch verification server closed. Reconnecting in 5 seconds..."
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    // Continue loop to retry
                }
            }
        }
    }
}

struct ChannelWriter {
    tx: PollSender<Result<Frame<Bytes>, io::Error>>,
}

impl ChannelWriter {
    fn new(tx: mpsc::Sender<Result<Frame<Bytes>, io::Error>>) -> Self {
        Self {
            tx: PollSender::new(tx),
        }
    }
}

impl AsyncWrite for ChannelWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match Pin::new(&mut self.tx).poll_reserve(cx) {
            Poll::Ready(Ok(())) => {
                let len = buf.len();
                let data = Bytes::copy_from_slice(buf);
                let frame = Frame::data(data);

                if self.tx.send_item(Ok(frame)).is_err() {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "channel closed",
                    )));
                }
                Poll::Ready(Ok(len))
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "channel closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        self.tx.close();
        Poll::Ready(Ok(()))
    }
}
