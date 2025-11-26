use crate::{
    BATCH_VERIFICATION_WIRE_FORMAT_VERSION, BatchVerificationRequest,
    BatchVerificationRequestCodec, BatchVerificationResponse, BatchVerificationResponseDecoder,
};
use futures::{SinkExt, StreamExt};
use tokio::io::BufReader;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::{FramedRead, FramedWrite};
use zksync_os_l1_sender::batcher_model::BatchForSigning;
use zksync_os_socket::skip_http_headers;

/// Accepts connections from batch verification clients. Crafts and sends
/// BatchVerificationRequests to all clients. Receives responses and forwards
/// them through the channel to batch_response_processor
pub(super) struct BatchVerificationServer {
    verification_request_broadcast: broadcast::Sender<BatchVerificationRequest>,
    response_sender: mpsc::Sender<BatchVerificationResponse>,
}

#[derive(Debug, thiserror::Error)]
#[allow(clippy::large_enum_variant)]
pub enum BatchVerificationRequestError {
    #[error("Not enough clients connected: {0} < {1}")]
    NotEnoughClients(usize, usize),
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
    pub async fn run_server(&self, address: String) -> anyhow::Result<()> {
        tracing::info!("Starting Batch Verification server at {}", address);

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
                    tracing::info!("Error handling client {}: {}", addr, e);
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
        send.write_u32(BATCH_VERIFICATION_WIRE_FORMAT_VERSION)
            .await?;

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
                                tracing::error!(
                                    batch_number = e.0.batch_number,
                                    request_id = e.0.request_id,
                                    "Failed to forward response from client {}: {}", client_addr, e
                                );
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
            pubdata_mode: batch_envelope.batch.pubdata_mode,
            commit_data: batch_envelope.batch.batch_info.commit_info.clone(),
            request_id,
        };

        let clients_count = self.verification_request_broadcast.receiver_count();

        if clients_count < required_clients {
            return Err(BatchVerificationRequestError::NotEnoughClients(
                clients_count,
                required_clients,
            ));
        }

        self.verification_request_broadcast.send(request)?;

        tracing::info!(
            request_id,
            batch_number = batch_envelope.batch_number(),
            "Sent batch verification request to {} clients",
            clients_count,
        );

        Ok(())
    }
}

#[cfg(test)]
impl BatchVerificationServer {
    pub fn subscribe_for_tests(&self) -> broadcast::Receiver<BatchVerificationRequest> {
        self.verification_request_broadcast.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::dummy_batch_envelope;

    #[tokio::test]
    async fn send_verification_request_errors_on_not_enough_clients() {
        let (server, _responses) = BatchVerificationServer::new();
        let batch_envelope = dummy_batch_envelope(1, 1, 5);

        let result = server
            .send_verification_request(&batch_envelope, 42, 1)
            .await;

        match result {
            Err(BatchVerificationRequestError::NotEnoughClients(clients, required)) => {
                assert_eq!(clients, 0);
                assert_eq!(required, 1);
            }
            _ => panic!("Expected NotEnoughClients error"),
        }
    }

    #[tokio::test]
    async fn send_verification_request_sends_to_all_clients() {
        let (server, _responses) = BatchVerificationServer::new();
        let batch_envelope = dummy_batch_envelope(7, 10, 20);

        let mut rx = server.verification_request_broadcast.subscribe();

        let send_fut = server.send_verification_request(&batch_envelope, 5, 1);

        let recv_fut = async {
            let req = rx.recv().await.expect("expected request");
            assert_eq!(req.batch_number, 7);
            assert_eq!(req.first_block_number, 10);
            assert_eq!(req.last_block_number, 20);
            assert_eq!(req.pubdata_mode, batch_envelope.batch.pubdata_mode);
            assert_eq!(req.commit_data, batch_envelope.batch.batch_info.commit_info);
            assert_eq!(req.request_id, 5);
        };

        let _ = tokio::join!(send_fut, recv_fut);
    }
}
