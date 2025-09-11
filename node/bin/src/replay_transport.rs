use alloy::primitives::BlockNumber;
use backon::{ConstantBuilder, Retryable};
use futures::{SinkExt, StreamExt, stream::BoxStream};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::net::ToSocketAddrs;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::{self, FramedRead, FramedWrite, LengthDelimitedCodec};
use zksync_os_sequencer::model::blocks::BlockCommand;
use zksync_os_storage_api::{REPLAY_WIRE_FORMAT_VERSION, ReplayRecord};

use crate::block_replay_storage::BlockReplayStorage;

pub async fn replay_server(
    block_replays: BlockReplayStorage,
    address: impl ToSocketAddrs,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(address).await?;

    loop {
        let (mut socket, _) = listener.accept().await?;

        let block_replays = block_replays.clone();
        tokio::spawn(async move {
            let (recv, mut send) = socket.split();

            let mut reader = BufReader::new(recv);
            skip_http_headers(&mut reader)
                .await
                .expect("failed to skip HTTP headers");

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
            let mut stream = block_replays.replay_commands_forever(starting_block);
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

async fn skip_http_headers<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<(), std::io::Error> {
    // Detects two consecutive line endings, which may be \r\n or \n.
    let mut empty_line = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF reached before end of headers",
            ));
        }

        for (i, &byte) in buf.iter().enumerate() {
            if byte == b'\n' {
                if empty_line {
                    reader.consume(i + 1);
                    return Ok(());
                }
                empty_line = true;
            } else if byte != b'\r' {
                empty_line = false;
            }
        }

        let len = buf.len();
        reader.consume(len);
    }
}

pub async fn replay_receiver(
    starting_block: BlockNumber,
    address: impl ToSocketAddrs,
) -> anyhow::Result<BoxStream<'static, BlockCommand>> {
    let mut socket = (|| TcpStream::connect(&address))
        .retry(
            ConstantBuilder::default()
                .with_delay(Duration::from_secs(1))
                .with_max_times(10),
        )
        .notify(|err, dur| {
            tracing::warn!(?err, ?dur, "retrying connection to main node");
        })
        .await?;

    // This makes it valid HTTP
    socket
        .write_all(b"POST /block_replays HTTP/1.0\r\n\r\n")
        .await?;

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
