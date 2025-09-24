use alloy::primitives::BlockNumber;
use backon::{ConstantBuilder, Retryable};
use futures::{SinkExt, StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::net::ToSocketAddrs;
use tokio::sync::{Mutex, mpsc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::{self, FramedRead, FramedWrite, LengthDelimitedCodec};
use zksync_os_l1_sender::batcher_model::{BatchEnvelope, ProverInput};

use crate::batch_verification_wire_format::BATCH_VERIFICATION_WIRE_FORMAT_VERSION;

/// Request sent from main sequencer to external nodes for batch verification
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchVerificationRequest {
    pub batch_number: u64,
    pub first_block_number: u64,
    pub last_block_number: u64,
    pub request_id: u64,
}

/// Response sent from external nodes back to main sequencer
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchVerificationResponse {
    pub request: BatchVerificationRequest,
    pub signature: Vec<u8>, // TODO better type Placeholder for signature bytes
}

/// Manages connected clients and collects their responses
pub struct BatchVerificationServer {
    clients: Arc<Mutex<HashMap<String, mpsc::Sender<BatchVerificationRequest>>>>,
    response_sender: mpsc::Sender<BatchVerificationResponse>,
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
    pub async fn start_server(&self, address: impl ToSocketAddrs) -> anyhow::Result<()> {
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
        let (recv, send) = socket.split();
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

        let mut writer = FramedWrite::new(send, BatchVerificationCodec::new());
        let mut reader = FramedRead::new(reader, BatchVerificationCodec::new());

        // Handle bidirectional communication
        loop {
            tokio::select! {
                // Send requests to client
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

                // Receive responses from client
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
    pub async fn send_verification_request(
        &self,
        batch_envelope: &BatchEnvelope<ProverInput>,
        request_id: u64,
    ) -> anyhow::Result<()> {
        let request = BatchVerificationRequest {
            batch_number: batch_envelope.batch_number(),
            first_block_number: batch_envelope.batch.first_block_number,
            last_block_number: batch_envelope.batch.last_block_number,
            request_id,
        };

        let clients = self.clients.lock().await;
        let mut send_tasks = Vec::new();

        for (client_id, sender) in clients.iter() {
            let client_id = client_id.clone();
            let request = request.clone();
            let sender = sender.clone();

            send_tasks.push(tokio::spawn(async move {
                if let Err(e) = sender.send(request).await {
                    tracing::error!(
                        "Failed to send verification request to client {}: {}",
                        client_id,
                        e
                    );
                }
            }));
        }

        // Wait for all sends to complete
        for task in send_tasks {
            let _ = task.await;
        }

        tracing::info!(
            "Sent batch verification request {} to {} clients",
            request_id,
            clients.len()
        );

        Ok(())
    }

    /// Collect responses for a specific request
    pub async fn collect_responses(
        &mut self,
        response_receiver: &mut mpsc::Receiver<BatchVerificationResponse>,
        request_id: u64,
        timeout: Duration,
    ) -> Vec<BatchVerificationResponse> {
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;

        while tokio::time::Instant::now() < deadline {
            let remaining_time = deadline - tokio::time::Instant::now();

            match tokio::time::timeout(remaining_time, response_receiver.recv()).await {
                Ok(Some(response)) => {
                    if response.request.request_id == request_id {
                        responses.push(response);
                    }
                    // Note: responses for other requests are dropped here
                    // In a production system, you'd want to buffer them
                }
                Ok(None) => break, // Channel closed
                Err(_) => break,   // Timeout
            }
        }

        responses
    }
}

/// Client that connects to the main sequencer for batch verification
pub struct BatchVerificationClient {
    node_id: String,
    private_key: Vec<u8>, // Placeholder for signing key
}

impl BatchVerificationClient {
    pub fn new(node_id: String, private_key: Vec<u8>) -> Self {
        Self {
            node_id,
            private_key,
        }
    }

    /// Connect to the main sequencer and handle verification requests
    pub async fn connect_and_handle(&self, address: impl ToSocketAddrs) -> anyhow::Result<()> {
        let mut socket = (|| TcpStream::connect(&address))
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

        // Send HTTP headers similar to replay_transport
        socket
            .write_all(b"POST /batch_verification HTTP/1.0\r\n\r\n")
            .await?;

        let (recv, send) = socket.split();
        let mut reader = FramedRead::new(recv, BatchVerificationRequestCodec::new());
        let mut writer = FramedWrite::new(send, BatchVerificationResponseCodec::new());

        tracing::info!("Connected to main sequencer for batch verification");

        while let Some(message) = reader.next().await {
            let response = self.handle_verification_request(message?).await?;
            writer.send(response).await?;
        }

        Ok(())
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
            request.last_block_number
        );

        // TODO: Implement actual batch verification logic
        // For now, create a dummy signature
        let signature = self.sign_batch_verification(&request).await?;

        Ok(BatchVerificationResponse { request, signature })
    }

    async fn sign_batch_verification(
        &self,
        request: &BatchVerificationRequest,
    ) -> anyhow::Result<Vec<u8>> {
        // TODO: Implement actual cryptographic signing
        // For now, return a dummy signature based on node_id and request data
        let signature_data = format!(
            "{}:{}:{}:{}:{}",
            self.node_id,
            request.batch_number,
            request.first_block_number,
            request.last_block_number,
            request.request_id
        );

        Ok(signature_data.into_bytes())
    }
}

/// Codecs for encoding/decoding batch verification messages

struct BatchVerificationRequestDecoder {
    inner: LengthDelimitedCodec,
    wire_format_version: u32,
}

impl BatchVerificationRequestDecoder {
    fn new(wire_format_version: u32) -> Self {
        Self {
            inner: LengthDelimitedCodec::new(),
            wire_format_version,
        }
    }
}

impl codec::Decoder for BatchVerificationRequestDecoder {
    type Item = BatchVerificationRequest;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        src: &mut alloy::rlp::BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        self.inner.decode(src).map(|inner| {
            inner.map(|bytes| BatchVerificationRequest::decode(&bytes, self.wire_format_version))
        })
    }
}

pub struct BatchVerificationRequestCodec(LengthDelimitedCodec);

impl BatchVerificationRequestCodec {
    pub fn new() -> Self {
        Self(LengthDelimitedCodec::new())
    }
}

impl codec::Encoder<BatchVerificationRequest> for BatchVerificationRequestCodec {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        item: BatchVerificationRequest,
        dst: &mut alloy::rlp::BytesMut,
    ) -> Result<(), Self::Error> {
        self.0
            .encode(item.encode_with_current_version().into(), dst)
    }
}

struct BatchVerificationResponseDecoder {
    inner: LengthDelimitedCodec,
    wire_format_version: u32,
}

impl BatchVerificationResponseDecoder {
    fn new(wire_format_version: u32) -> Self {
        Self {
            inner: LengthDelimitedCodec::new(),
            wire_format_version,
        }
    }
}

impl codec::Decoder for BatchVerificationResponseDecoder {
    type Item = BatchVerificationResponse;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        src: &mut alloy::rlp::BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        self.inner.decode(src).map(|inner| {
            inner.map(|bytes| BatchVerificationResponse::decode(&bytes, self.wire_format_version))
        })
    }
}

pub struct BatchVerificationResponseCodec(LengthDelimitedCodec);

impl BatchVerificationResponseCodec {
    pub fn new() -> Self {
        Self(LengthDelimitedCodec::new())
    }
}

impl codec::Encoder<BatchVerificationResponse> for BatchVerificationResponseCodec {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        item: BatchVerificationResponse,
        dst: &mut alloy::rlp::BytesMut,
    ) -> Result<(), Self::Error> {
        self.0
            .encode(item.encode_with_current_version().into(), dst)
    }
}
