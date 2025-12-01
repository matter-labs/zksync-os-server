use std::fmt::Display;

use alloy::primitives::{BlockNumber, Bytes};
use futures::{SinkExt, StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;
use tokio::net::ToSocketAddrs;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::codec::{self, FramedRead, FramedWrite, LengthDelimitedCodec};
use zksync_os_sequencer::model::blocks::BlockCommand;
use zksync_os_socket::{connect, skip_http_headers};
use zksync_os_storage_api::{REPLAY_WIRE_FORMAT_VERSION, ReadReplay, ReadReplayExt, ReplayRecord};

pub async fn replay_server(
    block_replays: impl ReadReplay + Clone,
    address: impl ToSocketAddrs,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(address).await?;

    loop {
        let (mut socket, _) = listener.accept().await?;

        let block_replays = block_replays.clone();
        tokio::spawn(async move {
            let (recv, mut send) = socket.split();

            let mut reader = BufReader::new(recv);
            let _skipped_bytes = skip_http_headers(&mut reader)
                .await
                .expect("failed to skip HTTP headers");

            let len = match reader.read_u32().await {
                Ok(len) => len,
                Err(e) => {
                    tracing::info!("Could not read query len: {}", e);
                    return;
                }
            };

            let mut buf = vec![0u8; len as usize];
            if let Err(e) = reader.read_exact(&mut buf).await {
                tracing::info!("Could not read query bytes: {}", e);
                return;
            };

            let block_replay_query: BlockReplayQuery = match serde_json::from_slice(&buf) {
                Ok(q) => q,
                Err(e) => {
                    tracing::info!("Could not deserialize query: {}", e);
                    return;
                }
            };

            if let Err(e) = send.write_u32(REPLAY_WIRE_FORMAT_VERSION).await {
                tracing::info!("Could not write replay version: {}", e);
                return;
            }

            tracing::info!(
                "Streaming replays to {} starting from {}",
                send.peer_addr().unwrap(),
                block_replay_query.starting_block
            );

            let mut replay_sender = FramedWrite::new(send, BlockReplayEncoder::new());
            let mut stream = block_replays.stream_from_forever(
                block_replay_query.starting_block,
                block_replay_query
                    .record_overrides
                    .into_iter()
                    .map(|(k, v)| (k, v.to_vec()))
                    .collect(),
            );
            loop {
                let replay = stream.next().await.unwrap();
                match replay_sender.send(replay).await {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::info!("Failed to send replay: {}", e);
                        return;
                    }
                };
            }
        });
    }
}

pub async fn replay_receiver(
    starting_block: BlockNumber,
    record_overrides: Vec<(u64, Bytes)>,
    address: impl ToSocketAddrs + Display,
) -> anyhow::Result<BoxStream<'static, BlockCommand>> {
    let query = BlockReplayQuery::new(starting_block, record_overrides);
    let path = "/block_replays";
    let mut socket = connect(&address, path).await?;

    // Instead of negotiating an upgrade, we just drop down to the TCP layer after the headers.
    let query_bytes = serde_json::to_vec(&query)?;
    socket.write_u32(query_bytes.len().try_into()?).await?;
    socket.write_all(&query_bytes).await?;
    let replay_version = socket.read_u32().await?;

    Ok(
        FramedRead::new(socket, BlockReplayDecoder::new(replay_version))
            .map(|replay| BlockCommand::Replay(Box::new(replay.unwrap())))
            .boxed(),
    )
}

struct BlockReplayDecoder {
    inner: LengthDelimitedCodec,
    wire_format_version: u32,
}

impl BlockReplayDecoder {
    fn new(wire_format_version: u32) -> Self {
        Self {
            inner: LengthDelimitedCodec::new(),
            wire_format_version,
        }
    }
}

impl codec::Decoder for BlockReplayDecoder {
    type Item = ReplayRecord;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        src: &mut alloy::rlp::BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        self.inner
            .decode(src)
            .map(|inner| inner.map(|bytes| ReplayRecord::decode(&bytes, self.wire_format_version)))
    }
}

struct BlockReplayEncoder(LengthDelimitedCodec);

impl BlockReplayEncoder {
    fn new() -> Self {
        Self(LengthDelimitedCodec::new())
    }
}

impl codec::Encoder<ReplayRecord> for BlockReplayEncoder {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        item: ReplayRecord,
        dst: &mut alloy::rlp::BytesMut,
    ) -> Result<(), Self::Error> {
        self.0
            .encode(item.encode_with_current_version().into(), dst)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BlockReplayQuery {
    starting_block: u64,
    record_overrides: Vec<(u64, Bytes)>,
}

impl BlockReplayQuery {
    pub fn new(starting_block: u64, record_overrides: Vec<(u64, Bytes)>) -> Self {
        Self {
            starting_block,
            record_overrides,
        }
    }
}
