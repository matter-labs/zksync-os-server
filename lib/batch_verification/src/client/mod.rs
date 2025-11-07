use crate::{
    BatchVerificationRequest, BatchVerificationRequestDecoder, BatchVerificationResponse,
    BatchVerificationResponseCodec, BatchVerificationResult,
};
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use std::str::FromStr;
use std::time::Duration;
use structdiff::StructDiff;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};
use zksync_os_batch_types::BatchSignature;
use zksync_os_batch_types::BlockMerkleTreeData;
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::commitment::BatchInfo;
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_observability::ComponentStateHandle;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_observability::GenericComponentState;
use zksync_os_observability::StateLabel;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_socket::connect;
use zksync_os_storage_api::ReadFinality;
use zksync_os_storage_api::ReplayRecord;

mod block_cache;
mod metrics;

use block_cache::BlockCache;

/// Client that connects to the main sequencer for batch verification
pub struct BatchVerificationClient<Finality> {
    chain_id: u64,
    diamond_proxy: Address,
    server_address: String,
    signer: PrivateKeySigner,
    block_cache: BlockCache<Finality>,
}

#[derive(Debug, thiserror::Error)]
enum BatchVerificationError {
    #[error("Missing records for block {0}")]
    MissingBlock(u64),
    #[error("Tree error")]
    TreeError,
    #[error("Batch data mismatch: {0}")]
    BatchDataMismatch(String),
}

type VerificationInput = (
    BlockOutput,
    zksync_os_storage_api::ReplayRecord,
    BlockMerkleTreeData,
);

impl<Finality: ReadFinality> BatchVerificationClient<Finality> {
    pub fn new(
        finality: Finality,
        private_key: SecretString,
        chain_id: u64,
        diamond_proxy: Address,
        server_address: String,
    ) -> Self {
        Self {
            signer: PrivateKeySigner::from_str(private_key.expose_secret())
                .expect("Invalid batch verification private key"),
            chain_id,
            diamond_proxy,
            block_cache: BlockCache::new(finality),
            server_address,
        }
    }

    async fn connect_and_handle(
        &mut self,
        input: &mut PeekableReceiver<VerificationInput>,
        latency_tracker: &ComponentStateHandle<BatchVerificationClientState>,
    ) -> anyhow::Result<()> {
        let mut socket = connect(&self.server_address, "/batch_verification").await?;

        let batch_verification_version = socket.read_u32().await?;
        let (recv, send) = socket.split();
        let mut reader = FramedRead::new(
            recv,
            BatchVerificationRequestDecoder::new(batch_verification_version),
        );
        let mut writer = FramedWrite::new(
            send,
            BatchVerificationResponseCodec::new(batch_verification_version),
        );

        tracing::info!("Connected to main sequencer for batch verification");

        loop {
            latency_tracker.enter_state(BatchVerificationClientState::WaitingRecv);
            tokio::select! {
                block = input.recv() => {
                    match block {
                        Some((block_output, replay_record, tree_data)) => {
                            // we remove blocks from cache based on incoming singing requests.
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
                                    tracing::info!(batch_number, request_id, "Approved batch verification request");
                                    writer.send(BatchVerificationResponse { request_id, batch_number, result: BatchVerificationResult::Success(signature) }).await?;
                                },
                                Err(reason) => {
                                    tracing::info!(batch_number, request_id, "Batch verification failed: {}", reason);
                                    writer.send(BatchVerificationResponse { request_id, batch_number, result: BatchVerificationResult::Refused(reason.to_string()) }).await?;
                                },
                            }
                        }
                        Some(Err(parsing_err)) =>
                        {
                            tracing::error!("Error parsing verification request message. Ignoring: {}", parsing_err);
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

        let commit_batch_info = BatchInfo::new(
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
            self.diamond_proxy,
            request.batch_number,
        )
        .commit_info;

        if commit_batch_info != request.commit_data {
            let diff = request.commit_data.diff(&commit_batch_info);

            return Err(BatchVerificationError::BatchDataMismatch(format!(
                "Batch data mismatch: {diff:?}",
            )));
        }

        let signature = BatchSignature::sign_batch(&request.commit_data, &self.signer).await;

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
impl<Finality: ReadFinality> PipelineComponent for BatchVerificationClient<Finality> {
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
