use alloy::primitives::{BlockNumber, Bytes};
use anyhow::anyhow;
use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::{SinkExt, StreamExt, TryStreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::ToSocketAddrs;
use tokio::{io::AsyncReadExt, net::TcpListener};
use tokio_util::codec::{self, FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_util::io::{ReaderStream, StreamReader};
use zksync_os_sequencer::model::blocks::BlockCommand;
use zksync_os_storage_api::{REPLAY_WIRE_FORMAT_VERSION, ReadReplay, ReadReplayExt, ReplayRecord};

async fn block_replays_handler(
    State(state): State<Arc<dyn ReadReplay>>,
    Json(block_replay_query): Json<BlockReplayQuery>,
) -> Response {
    let (mut tx, rx) = tokio::io::duplex(16 * 1024);

    tokio::spawn(async move {
        if let Err(e) = tx.write_u32(REPLAY_WIRE_FORMAT_VERSION).await {
            tracing::info!("Could not write replay version: {}", e);
            return;
        }

        tracing::info!(
            starting_block = block_replay_query.starting_block,
            "streaming replay records",
        );

        let mut replay_sender = FramedWrite::new(tx, BlockReplayEncoder::new());
        let mut stream = state.stream_from_forever(
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
                    tracing::info!("failed to send replay: {}", e);
                    return;
                }
            };
        }
    });

    let body = Body::from_stream(ReaderStream::new(rx));
    body.into_response()
}

pub async fn replay_server(
    block_replays: impl ReadReplay + Clone,
    address: impl ToSocketAddrs,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(address).await?;
    let app = Router::new()
        .route("/block_replays", post(block_replays_handler))
        .with_state(Arc::new(block_replays));
    axum::serve(listener, app).await?;

    Ok(())
}

pub async fn replay_receiver(
    starting_block: BlockNumber,
    record_overrides: Vec<(u64, Bytes)>,
    address: &str,
) -> anyhow::Result<BoxStream<'static, BlockCommand>> {
    let client = reqwest::Client::new();

    let query = BlockReplayQuery::new(starting_block, record_overrides);
    let response = client
        .post(format!("{address}/block_replays"))
        .body(serde_json::to_vec(&query)?)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .send()
        .await?;
    if !response.status().is_success() {
        let text = response.text().await?;
        return Err(anyhow!("request failed: {text}"));
    }

    let stream = response.bytes_stream();
    let stream = stream.map_err(std::io::Error::other);
    let mut reader = StreamReader::new(stream);
    let replay_version = reader.read_u32().await?;
    tracing::info!(replay_version, "start streaming from main node");

    Ok(
        FramedRead::new(reader, BlockReplayDecoder::new(replay_version))
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
