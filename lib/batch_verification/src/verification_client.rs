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
use zksync_os_l1_sender::commitment::BatchInfo;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::{io::AsyncWriteExt, net::TcpStream};
use tokio_util::codec::{FramedRead, FramedWrite};
use zksync_os_interface::types::BlockOutput;
use zksync_os_merkle_tree::BlockMerkleTreeData;
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::BatchSignature;

/// Client that connects to the main sequencer for batch verification
pub struct BatchVerificationClient {
    chain_id: u64,
    chain_address: Address,
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
        chain_address: Address,
        server_address: String,
    ) -> Self {
        Self {
            signer: PrivateKeySigner::from_str(private_key.expose_secret())
                .expect("Invalid batch verification private key"),
            chain_id,
            chain_address,
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
            self.chain_address,
            request.batch_number,
        )
        .commit_info;

        if commit_batch_info != request.commit_data {
            let mut mismatches = Vec::new();

            // I don't like this, but it works great :/
            if commit_batch_info.batchNumber != request.commit_data.batchNumber {
                mismatches.push(format!(
                    "batchNumber: local={}, remote={}",
                    commit_batch_info.batchNumber, request.commit_data.batchNumber
                ));
            }
            if commit_batch_info.newStateCommitment != request.commit_data.newStateCommitment {
                mismatches.push(format!(
                    "newStateCommitment: local={:?}, remote={:?}",
                    commit_batch_info.newStateCommitment, request.commit_data.newStateCommitment
                ));
            }
            if commit_batch_info.numberOfLayer1Txs != request.commit_data.numberOfLayer1Txs {
                mismatches.push(format!(
                    "numberOfLayer1Txs: local={}, remote={}",
                    commit_batch_info.numberOfLayer1Txs, request.commit_data.numberOfLayer1Txs
                ));
            }
            if commit_batch_info.priorityOperationsHash
                != request.commit_data.priorityOperationsHash
            {
                mismatches.push(format!(
                    "priorityOperationsHash: local={:?}, remote={:?}",
                    commit_batch_info.priorityOperationsHash,
                    request.commit_data.priorityOperationsHash
                ));
            }
            if commit_batch_info.dependencyRootsRollingHash
                != request.commit_data.dependencyRootsRollingHash
            {
                mismatches.push(format!(
                    "dependencyRootsRollingHash: local={:?}, remote={:?}",
                    commit_batch_info.dependencyRootsRollingHash,
                    request.commit_data.dependencyRootsRollingHash
                ));
            }
            if commit_batch_info.l2LogsTreeRoot != request.commit_data.l2LogsTreeRoot {
                mismatches.push(format!(
                    "l2LogsTreeRoot: local={:?}, remote={:?}",
                    commit_batch_info.l2LogsTreeRoot, request.commit_data.l2LogsTreeRoot
                ));
            }
            if commit_batch_info.l2DaValidator != request.commit_data.l2DaValidator {
                mismatches.push(format!(
                    "l2DaValidator: local={:?}, remote={:?}",
                    commit_batch_info.l2DaValidator, request.commit_data.l2DaValidator
                ));
            }
            if commit_batch_info.daCommitment != request.commit_data.daCommitment {
                mismatches.push(format!(
                    "daCommitment: local={:?}, remote={:?}",
                    commit_batch_info.daCommitment, request.commit_data.daCommitment
                ));
            }
            if commit_batch_info.firstBlockTimestamp != request.commit_data.firstBlockTimestamp {
                mismatches.push(format!(
                    "firstBlockTimestamp: local={}, remote={}",
                    commit_batch_info.firstBlockTimestamp, request.commit_data.firstBlockTimestamp
                ));
            }
            if commit_batch_info.lastBlockTimestamp != request.commit_data.lastBlockTimestamp {
                mismatches.push(format!(
                    "lastBlockTimestamp: local={}, remote={}",
                    commit_batch_info.lastBlockTimestamp, request.commit_data.lastBlockTimestamp
                ));
            }
            if commit_batch_info.chainId != request.commit_data.chainId {
                mismatches.push(format!(
                    "chainId: local={}, remote={}",
                    commit_batch_info.chainId, request.commit_data.chainId
                ));
            }
            if commit_batch_info.operatorDAInput != request.commit_data.operatorDAInput {
                mismatches.push(format!(
                    "operatorDAInput: local={} bytes, remote={} bytes",
                    commit_batch_info.operatorDAInput.len(),
                    request.commit_data.operatorDAInput.len()
                ));
            }

            return Err(BatchVerificationError::BatchDataMismatch(format!(
                "Batch data mismatch - {} field(s) differ: {}",
                mismatches.len(),
                mismatches.join("; ")
            )));
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
        let replay_version = socket.read_u32().await?;
        let (recv, send) = socket.split();
        let mut reader =
            FramedRead::new(recv, BatchVerificationRequestDecoder::new(replay_version));
        let mut writer =
            FramedWrite::new(send, BatchVerificationResponseCodec::new(replay_version));

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
