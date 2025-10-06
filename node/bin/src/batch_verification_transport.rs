use alloy::primitives::Address;
use alloy::signers::Signer;
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use backon::{ConstantBuilder, Retryable};
use futures::future::join_all;
use futures::{SinkExt, StreamExt};
use smart_config::value::{ExposeSecret, SecretString};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::ToSocketAddrs;
use tokio::sync::{Mutex, mpsc};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::{FramedRead, FramedWrite};
use zksync_os_batch_verification::{
    BATCH_VERIFICATION_WIRE_FORMAT_VERSION, BatchVerificationRequest,
    BatchVerificationRequestCodec, BatchVerificationRequestDecoder, BatchVerificationResponse,
    BatchVerificationResponseCodec, BatchVerificationResponseDecoder,
};
use zksync_os_interface::types::BlockOutput;
use zksync_os_l1_sender::batcher_model::BatchForSigning;
use zksync_os_l1_sender::commitment::BatchInfo;
use zksync_os_merkle_tree::TreeBatchOutput;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent};
use zksync_os_storage_api::ReplayRecord;
use zksync_os_storage_api::skip_http_headers;

use crate::tree_manager::BlockMerkleTreeData;

/// Manages connected clients and collects their responses
pub struct BatchVerificationServer {
    clients: Arc<Mutex<HashMap<String, mpsc::Sender<BatchVerificationRequest>>>>,
    response_sender: mpsc::Sender<BatchVerificationResponse>,
}

#[derive(Debug, thiserror::Error)]
pub enum BatchVerificationRequestError {
    #[error("Not enough clients connected")]
    NotEnoughClients,
}

impl BatchVerificationServer {
    pub fn new() -> (Self, mpsc::Receiver<BatchVerificationResponse>) {
        let (response_sender, response_receiver) = mpsc::channel(100);

        let server = Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            response_sender,
        };

        (server, response_receiver)
    }

    /// Start the TCP server that accepts connections from external nodes
    pub async fn run_server(&self, address: impl ToSocketAddrs) -> anyhow::Result<()> {
        let listener = TcpListener::bind(address).await?;
        let clients = self.clients.clone();
        let response_sender = self.response_sender.clone();

        loop {
            let (socket, addr) = listener.accept().await?;
            let clients = clients.clone();
            let response_sender = response_sender.clone();
            let client_addr = addr.to_string();

            tokio::spawn(async move {
                if let Err(e) =
                    Self::handle_client(socket, client_addr, clients, response_sender).await
                {
                    tracing::error!("Error handling client {}: {}", addr, e);
                }
            });
        }
    }

    async fn handle_client(
        mut socket: TcpStream,
        client_addr: String,
        clients: Arc<Mutex<HashMap<String, mpsc::Sender<BatchVerificationRequest>>>>,
        response_sender: mpsc::Sender<BatchVerificationResponse>,
    ) -> anyhow::Result<()> {
        let (recv, mut send) = socket.split();
        let mut reader = BufReader::new(recv);

        // Skip HTTP headers similar to replay_transport
        skip_http_headers(&mut reader).await?;

        // Write wire format version
        if let Err(e) = send.write_u32(BATCH_VERIFICATION_WIRE_FORMAT_VERSION).await {
            tracing::info!("Could not write batch verification version: {}", e);
            return Ok(());
        }

        let (request_sender, mut request_receiver) = mpsc::channel::<BatchVerificationRequest>(10);

        // Register this client
        {
            let mut clients_guard = clients.lock().await;
            clients_guard.insert(client_addr.clone(), request_sender);
        }

        tracing::info!("Batch verification client connected: {}", client_addr);

        let mut writer = FramedWrite::new(send, BatchVerificationRequestCodec::new());
        let mut reader = FramedRead::new(reader, BatchVerificationResponseDecoder::new());

        // Handle bidirectional communication
        loop {
            tokio::select! {
                // Send batches for signing to the client (verifier EN)
                request = request_receiver.recv() => {
                    match request {
                        Some(req) => {
                            if let Err(e) = writer.send(req).await {
                                tracing::error!("Failed to send request to client {}: {}", client_addr, e);
                                break;
                            }
                        }
                        None => break, // Channel closed
                    }
                }

                // Receive signing responses from client (verifier EN)
                response = reader.next() => {
                    match response {
                        Some(Ok(resp)) => {
                            if let Err(e) = response_sender.send(resp).await {
                                tracing::error!("Failed to forward response from client {}: {}", client_addr, e);
                            }
                        }
                        Some(Err(e)) => {
                            tracing::error!("Error reading from client {}: {}", client_addr, e);
                            break;
                        }
                        None => break, // Connection closed
                    }
                }
            }
        }

        // Cleanup client registration
        {
            let mut clients_guard = clients.lock().await;
            clients_guard.remove(&client_addr);
        }

        tracing::info!("Batch verification client disconnected: {}", client_addr);
        Ok(())
    }

    /// Send a batch verification request to all connected clients
    pub async fn send_verification_request<E: Sync>(
        &self,
        batch_envelope: &BatchForSigning<E>,
        request_id: u64,
        required_clients: usize,
    ) -> Result<(), BatchVerificationRequestError> {
        let request = BatchVerificationRequest {
            batch_number: batch_envelope.batch_number(),
            first_block_number: batch_envelope.batch.first_block_number,
            last_block_number: batch_envelope.batch.last_block_number,
            commit_data: batch_envelope.batch.batch_info.commit_info.clone(),
            request_id,
        };

        // Clone senders while holding the lock, then drop the lock before awaiting.
        let senders: Vec<(String, mpsc::Sender<BatchVerificationRequest>)> = {
            let clients = self.clients.lock().await;
            clients
                .iter()
                .map(|(id, tx)| (id.clone(), tx.clone()))
                .collect()
        };

        let senders_len = senders.len();
        if senders_len < required_clients {
            return Err(BatchVerificationRequestError::NotEnoughClients);
        }

        // Dispatch sends concurrently and await them together.
        let send_futs = senders
            .into_iter()
            .map(|(client_id, sender)| {
                let req = request.clone();
                async move {
                    if let Err(e) = sender.send(req).await {
                        tracing::error!(
                            "Failed to send verification request to client {}: {}",
                            client_id,
                            e
                        );
                        false
                    } else {
                        true
                    }
                }
            })
            .collect::<Vec<_>>();

        let _results = join_all(send_futs).await;

        tracing::info!(
            "Sent batch verification request {} for batch {} to {} clients",
            request_id,
            batch_envelope.batch_number(),
            senders_len
        );

        Ok(())
    }
}

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
    ) -> anyhow::Result<BatchVerificationResponse> {
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

        // For now, create a dummy signature. Think of something better in the future
        let signature = self.sign_batch_verification(&request).await?;

        Ok(BatchVerificationResponse {
            request_id: request.request_id,
            signature,
        })
    }

    async fn sign_batch_verification(
        &self,
        request: &BatchVerificationRequest,
    ) -> anyhow::Result<Vec<u8>> {
        // TODO: Implement actual cryptographic signing
        // For now, return a dummy signature based on request data
        let signature_data = format!(
            "{}:{}:{}:{}",
            request.batch_number,
            request.first_block_number,
            request.last_block_number,
            request.request_id
        );

        Ok(self
            .signer
            .sign_message(signature_data.as_bytes())
            .await?
            .into())
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
                        Some(message) => {
                            let response = self.handle_verification_request(message?).await?;
                            writer.send(response).await?;
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
