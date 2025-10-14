use crate::{
    BATCH_VERIFICATION_WIRE_FORMAT_VERSION, BatchVerificationRequest,
    BatchVerificationRequestCodec, BatchVerificationResponse, BatchVerificationResponseDecoder,
};
use futures::{SinkExt, StreamExt};
use tokio::io::BufReader;
use tokio::net::ToSocketAddrs;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::{FramedRead, FramedWrite};
use zksync_os_l1_sender::batcher_model::BatchForSigning;
use zksync_os_storage_api::skip_http_headers;

/// Manages connected clients and collects their responses
pub struct BatchVerificationServer {
    verification_request_broadcast: broadcast::Sender<BatchVerificationRequest>,
    response_sender: mpsc::Sender<BatchVerificationResponse>,
}

#[derive(Debug, thiserror::Error)]
#[allow(clippy::large_enum_variant)]
pub enum BatchVerificationRequestError {
    #[error("Not enough clients connected")]
    NotEnoughClients,
    #[error("Failed to send batch verification request: {0}")]
    SendError(#[from] broadcast::error::SendError<BatchVerificationRequest>),
}

impl BatchVerificationServer {
    pub fn new() -> (Self, mpsc::Receiver<BatchVerificationResponse>) {
        let (response_sender, response_receiver) = mpsc::channel(100);
        let (verification_request_broadcast, _rx_unused) = broadcast::channel(16);

        let server = Self {
            verification_request_broadcast,
            response_sender,
        };

        (server, response_receiver)
    }

    /// Start the TCP server that accepts connections from external nodes
    pub async fn run_server(&self, address: impl ToSocketAddrs) -> anyhow::Result<()> {
        let listener = TcpListener::bind(address).await?;
        let response_sender = self.response_sender.clone();

        loop {
            let (socket, addr) = listener.accept().await?;
            let verification_request_rx = self.verification_request_broadcast.subscribe();
            let response_sender = response_sender.clone();
            let client_addr = addr.to_string();

            tokio::spawn(async move {
                if let Err(e) = Self::handle_client(
                    socket,
                    client_addr,
                    verification_request_rx,
                    response_sender,
                )
                .await
                {
                    tracing::error!("Error handling client {}: {}", addr, e);
                }
            });
        }
    }

    async fn handle_client(
        mut socket: TcpStream,
        client_addr: String,
        mut verification_request_rx: broadcast::Receiver<BatchVerificationRequest>,
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

        tracing::info!("Batch verification client connected: {}", client_addr);

        let mut writer = FramedWrite::new(send, BatchVerificationRequestCodec::new());
        let mut reader = FramedRead::new(reader, BatchVerificationResponseDecoder::new());

        // Handle bidirectional communication
        loop {
            tokio::select! {
                // Send batches for signing to the client (verifier EN)
                request = verification_request_rx.recv() => {
                    match request {
                        Ok(req) => {
                            if let Err(e) = writer.send(req).await {
                                tracing::error!("Failed to send request to client {}: {}", client_addr, e);
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::error!("Error reading request for client {}: {}", client_addr, e);
                            break;
                        }
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

        let clients_count = self.verification_request_broadcast.receiver_count();

        if clients_count < required_clients {
            return Err(BatchVerificationRequestError::NotEnoughClients);
        }

        self.verification_request_broadcast.send(request)?;

        tracing::info!(
            "Sent batch verification request {} for batch {} to {} clients",
            request_id,
            batch_envelope.batch_number(),
            clients_count,
        );

        Ok(())
    }
}
