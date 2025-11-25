use std::fmt::Display;

use alloy::primitives::{BlockNumber, Bytes};
use futures::{SinkExt, StreamExt, stream::BoxStream};
use tokio::io::BufReader;
use tokio::net::ToSocketAddrs;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::codec::{self, FramedRead, FramedWrite, LengthDelimitedCodec};
use url::Url;
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
            let skipped_bytes = skip_http_headers(&mut reader)
                .await
                .expect("failed to skip HTTP headers");
            let block_replay_query = if let Ok(skipped_text) = String::from_utf8(skipped_bytes) {
                if let Some(url_str) = skipped_text
                    .split_whitespace()
                    .find(|s| s.starts_with("/block_replays"))
                {
                    if let Ok(url) = Url::parse(&format!("http://dummy{url_str}")) {
                        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
                        BlockReplayQuery::parse(pairs)
                    } else {
                        tracing::info!("Could not parse URL from HTTP headers");
                        BlockReplayQuery::default()
                    }
                } else {
                    tracing::info!("/block_replays not found in HTTP headers");
                    BlockReplayQuery::default()
                }
            } else {
                tracing::info!("Could not parse skipped HTTP headers as UTF-8");
                BlockReplayQuery::default()
            };

            let starting_block = match reader.read_u64().await {
                Ok(block_number) => block_number,
                Err(e) => {
                    tracing::info!("Could not read start block for replays: {}", e);
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
                starting_block
            );

            let mut replay_sender = FramedWrite::new(send, BlockReplayEncoder::new());
            let mut stream = block_replays.stream_from_forever(
                starting_block,
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
    let query = BlockReplayQuery::new(record_overrides);
    let query_string = query.query_string();
    let path = if query_string.is_empty() {
        "/block_replays".to_string()
    } else {
        format!("/block_replays?{}", query_string)
    };
    let mut socket = connect(&address, &path).await?;

    // Instead of negotiating an upgrade, we just drop down to the TCP layer after the headers.
    socket.write_u64(starting_block).await?;
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

#[derive(Debug, Clone, Default)]
struct BlockReplayQuery {
    record_overrides: Vec<(u64, Bytes)>,
}

impl BlockReplayQuery {
    pub fn new(record_overrides: Vec<(u64, Bytes)>) -> Self {
        Self { record_overrides }
    }

    pub fn query_string(&self) -> String {
        let mut query = String::new();
        for (block_number, override_type) in &self.record_overrides {
            if !query.is_empty() {
                query.push('&');
            }
            query.push_str(&format!("override_{block_number}={override_type}"));
        }
        query
    }

    pub fn parse(query_pairs: Vec<(String, String)>) -> Self {
        let mut record_overrides = Vec::new();
        for (key, value) in query_pairs {
            if let Some(suffix) = key.strip_prefix("override_")
                && let Ok(block_number) = suffix.parse::<u64>()
                && let Ok(bytes) = value.parse()
            {
                record_overrides.push((block_number, bytes));
            }
        }
        Self { record_overrides }
    }
}
