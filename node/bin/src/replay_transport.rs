use crate::block_replay_storage::BlockReplayStorage;
use alloy::primitives::BlockNumber;
use futures::{SinkExt, StreamExt, stream::BoxStream};
use tokio::net::ToSocketAddrs;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::{self, Framed, LengthDelimitedCodec};
use zksync_os_sequencer::model::blocks::BlockCommand;
use zksync_os_storage_api::ReplayRecord;

pub async fn replay_server(
    block_replays: BlockReplayStorage,
    address: impl ToSocketAddrs,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(address).await?;

    loop {
        let (mut socket, _) = listener.accept().await?;
        let starting_block = socket.read_u64().await.unwrap();

        let mut replay_sender = Framed::new(socket, BlockReplayCodec::new()).split().0;

        let block_replays = block_replays.clone();
        tokio::spawn(async move {
            let mut stream = block_replays.replay_commands_forever(starting_block);
            loop {
                let replay = stream.next().await.unwrap();
                replay_sender.send(replay).await.unwrap();
            }
        });
    }
}

pub async fn replay_receiver(
    starting_block: BlockNumber,
    address: impl ToSocketAddrs,
) -> BoxStream<'static, BlockCommand> {
    let mut socket = TcpStream::connect(address).await.unwrap();

    socket.write_u64(starting_block).await.unwrap();

    Framed::new(socket, BlockReplayCodec::new())
        .map(|replay| BlockCommand::Replay(Box::new(replay.unwrap())))
        .boxed()
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
                bincode::decode_from_slice(bytes.as_ref(), bincode::config::standard())
                    .unwrap()
                    .0
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
            bincode::encode_to_vec(item, bincode::config::standard())
                .unwrap()
                .into(),
            dst,
        )
    }
}
