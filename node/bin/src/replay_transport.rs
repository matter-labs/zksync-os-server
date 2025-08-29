use alloy::primitives::BlockNumber;
use backon::{ConstantBuilder, Retryable};
use futures::{SinkExt, StreamExt, stream::BoxStream};
use std::time::Duration;
use tokio::net::ToSocketAddrs;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::{self, Framed, LengthDelimitedCodec};
use zksync_os_sequencer::model::blocks::BlockCommand;
use zksync_os_storage_api::{
    PreviousReplayWireFormat, REPLAY_WIRE_FORMAT_VERSION, ReplayRecord, ReplayWireFormat,
};

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
            let starting_block = match socket.read_u64().await {
                Ok(block_number) => block_number,
                Err(e) => {
                    tracing::info!("Could not read start block for replays: {}", e);
                    return;
                }
            };
            if let Err(e) = socket.write_u32(REPLAY_WIRE_FORMAT_VERSION).await {
                tracing::info!("Could not write replay version: {}", e);
                return;
            }

            tracing::info!(
                "Streaming replays to {} starting from {}",
                socket.peer_addr().unwrap(),
                starting_block
            );

            let mut replay_sender = Framed::new(socket, BlockReplayCodec::new()).split().0;
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

    socket.write_u64(starting_block).await?;
    let replay_version = socket.read_u32().await?;

    Ok(if replay_version == REPLAY_WIRE_FORMAT_VERSION {
        Framed::new(socket, BlockReplayCodec::new())
            .map(|replay| BlockCommand::Replay(Box::new(replay.unwrap())))
            .boxed()
    } else if replay_version == REPLAY_WIRE_FORMAT_VERSION - 1 {
        Framed::new(socket, OldReplayCodec::new())
            .map(|replay| BlockCommand::Replay(Box::new(replay.unwrap())))
            .boxed()
    } else {
        panic!("Unsupported replay version: {replay_version}");
    })
}

struct BlockReplayCodec(LengthDelimitedCodec);

impl BlockReplayCodec {
    fn new() -> Self {
        Self(LengthDelimitedCodec::new())
    }
}

impl codec::Decoder for BlockReplayCodec {
    type Item = ReplayRecord;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        src: &mut alloy::rlp::BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        self.0.decode(src).map(|inner| {
            inner.map(|bytes| {
                let replay: ReplayWireFormat =
                    bincode::decode_from_slice(bytes.as_ref(), bincode::config::standard())
                        .unwrap()
                        .0;
                replay.into()
            })
        })
    }
}

impl codec::Encoder<ReplayRecord> for BlockReplayCodec {
    type Error = std::io::Error;

    fn encode(
        &mut self,
        item: ReplayRecord,
        dst: &mut alloy::rlp::BytesMut,
    ) -> Result<(), Self::Error> {
        self.0.encode(
            bincode::encode_to_vec(ReplayWireFormat::from(item), bincode::config::standard())
                .unwrap()
                .into(),
            dst,
        )
    }
}

struct OldReplayCodec(LengthDelimitedCodec);

impl OldReplayCodec {
    fn new() -> Self {
        Self(LengthDelimitedCodec::new())
    }
}

impl codec::Decoder for OldReplayCodec {
    type Item = ReplayRecord;
    type Error = std::io::Error;

    fn decode(
        &mut self,
        src: &mut alloy::rlp::BytesMut,
    ) -> Result<Option<Self::Item>, Self::Error> {
        self.0.decode(src).map(|inner| {
            inner.map(|bytes| {
                let old_replay: PreviousReplayWireFormat =
                    bincode::decode_from_slice(bytes.as_ref(), bincode::config::standard())
                        .unwrap()
                        .0;
                old_replay.into()
            })
        })
    }
}
