use crate::{
    BatchVerificationRequest, BatchVerificationRequestDecoder, BatchVerificationResponse,
    BatchVerificationResponseCodec, BatchVerificationResult,
};
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use backon::{ConstantBuilder, Retryable};
use futures::{SinkExt, StreamExt};
use smart_config::value::{ExposeSecret, SecretString};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::{io::AsyncWriteExt, net::TcpStream};
use tokio_util::codec::{FramedRead, FramedWrite};
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::commitment::BatchInfo;
use zksync_os_merkle_tree::BlockMerkleTreeData;
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::BatchSignature;

/// Client that connects to the main sequencer for batch verification
pub struct BatchVerificationClient {
    chain_id: u64,
    diamond_proxy: Address,
    signer: PrivateKeySigner, // TODO, we probably want to move to BLS?
    block_storage: HashMap<u64, (BlockOutput, ReplayRecord, BlockMerkleTreeData)>,
    server_address: String,
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

impl BatchVerificationClient {
    pub fn new(
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
            block_storage: HashMap::new(),
            server_address,
        }
    }

    async fn handle_verification_request(
        &self,
        request: BatchVerificationRequest,
    ) -> Result<BatchSignature, BatchVerificationError> {
        tracing::info!(
            "Handling batch verification request {} for batch {} (blocks {}-{})",
            request.request_id,
            request.batch_number,
            request.first_block_number,
            request.last_block_number,
        );

        let blocks: Vec<(&BlockOutput, &ReplayRecord, TreeBatchOutput)> =
            (request.first_block_number..=request.last_block_number)
                .map(|block_number| {
                    let (block_output, replay_record, tree_data) = self
                        .block_storage
                        .get(&block_number)
                        .ok_or(BatchVerificationError::MissingBlock(block_number))?;

                    let (root_hash, leaf_count) = tree_data
                        .block_start
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

        // TODO VALIDATE
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
            return Err(BatchVerificationError::BatchDataMismatch(
                "Batch data mismatch".to_string(), //TODO more info
            ));
        }

        let signature = BatchSignature::sign_batch(&request.commit_data, &self.signer).await;

        Ok(signature)
    }
}

#[async_trait]
impl PipelineComponent for BatchVerificationClient {
    type Input = (
        BlockOutput,
        zksync_os_storage_api::ReplayRecord,
        BlockMerkleTreeData,
    );
    type Output = ();

    const NAME: &'static str = "batch_verification_client";
    const OUTPUT_BUFFER_SIZE: usize = 5;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        _output: mpsc::Sender<Self::Output>,
    ) -> anyhow::Result<()> {
        let mut socket = (|| TcpStream::connect(&self.server_address))
            .retry(
                ConstantBuilder::default()
                    .with_delay(Duration::from_secs(1))
                    .with_max_times(10),
            )
            .notify(|err, dur| {
                tracing::warn!(
                    ?err,
                    ?dur,
                    "retrying connection to main node for batch verification"
                );
            })
            .await?;

        // This makes it valid HTTP
        socket
            .write_all(b"POST /batch_verification HTTP/1.0\r\n\r\n")
            .await?;

        // After HTTP headers we drop directly to simple TCP
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
            tokio::select! {
                block = input.recv() => {
                    match block {
                        Some((block_output, replay_record, tree_data)) => {
                            // TODO remove old blocks from storage
                            self.block_storage.insert(
                                replay_record.block_context.block_number,
                                (block_output, replay_record, tree_data),
                            );
                        }
                        None => break, // Channel closed, we are stopping now
                    }
                }
                // Handling in sequence without concurrency is fine as we shouldn't get too many requests and they should handle fast
                server_message = reader.next() => {
                    match server_message {
                        Some(Ok(message)) => {
                            //TODO a way to send errors
                            let batch_number = message.batch_number;
                            match self.handle_verification_request(message).await {
                                Ok(signature) => {
                                    tracing::info!("Approved batch verification request for {}", batch_number);
                                    writer.send(BatchVerificationResponse { request_id: batch_number, result: BatchVerificationResult::Success(signature) }).await?;
                                },
                                Err(reason) => {
                                    tracing::info!("Batch {} verification failed: {}", batch_number, reason);
                                    writer.send(BatchVerificationResponse { request_id: batch_number, result: BatchVerificationResult::Refused(reason.to_string()) }).await?;
                                },
                            }
                        }
                        Some(Err(parsing_err)) =>
                        {
                            tracing::error!("Error parsing verfication request message. Ignoring: {}", parsing_err);
                        }
                        None => {
                            tracing::error!("Server has disconnected verification client"); //TODO retries
                            break; // Connection closed
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
