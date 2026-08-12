//! Definitions for `zks` wire-protocol messages and version-aware encode / decode helpers.

use crate::version::ZksProtocolVersionSpec;
use crate::wire::{BlockReplays, GetBlockReplays, replays::RecordOverride};
use alloy::primitives::{
    BlockNumber,
    bytes::{Buf, BufMut, BytesMut},
};
use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use reth_eth_wire::protocol::Protocol;
use reth_network::types::Capability;
use std::fmt::Debug;
use zksync_os_storage_api::ReplayRecord;

pub const ZKS_PROTOCOL: &str = "zks";

/// Number of message types in the `zks` protocol.
const ZKS_MESSAGE_COUNT: u8 = 2;

/// A `zks` wire-protocol message.
///
/// The `zks` protocol is replay-only: verifier authentication and batch verification live in the
/// standalone `zks_2fa` subprotocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZksMessage<P: ZksProtocolVersionSpec> {
    /// Represents a `GetBlockReplays` streaming request.
    GetBlockReplays(GetBlockReplays),
    /// Represents a `BlockReplays` streaming response (one of many).
    BlockReplays(BlockReplays<P::Record>),
}

impl<P: ZksProtocolVersionSpec> ZksMessage<P> {
    /// Returns the capability for the zks protocol.
    pub const fn capability() -> Capability {
        Capability::new_static(ZKS_PROTOCOL, P::VERSION as usize)
    }

    /// Returns the protocol for the zks protocol.
    pub const fn protocol() -> Protocol {
        Protocol::new(Self::capability(), ZKS_MESSAGE_COUNT)
    }

    /// Returns the message's ID.
    pub const fn message_id(&self) -> ZksMessageId {
        match self {
            ZksMessage::GetBlockReplays(_) => ZksMessageId::GetBlockReplays,
            ZksMessage::BlockReplays(_) => ZksMessageId::BlockReplays,
        }
    }

    pub fn get_block_replays(
        starting_block: BlockNumber,
        max_blocks_per_message: Option<u64>,
        record_overrides: Vec<RecordOverride>,
    ) -> Self {
        Self::GetBlockReplays(GetBlockReplays {
            starting_block,
            max_blocks_per_message,
            record_overrides,
        })
    }

    pub fn block_replays(records: Vec<ReplayRecord>) -> Self {
        Self::BlockReplays(BlockReplays::new(records))
    }

    /// Return RLP encoded message.
    pub fn encoded(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(self.length());
        self.encode(&mut buf);
        buf
    }

    /// Decodes a `ZksMessage` from the given message buffer.
    pub fn decode_message(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let message_type = ZksMessageId::decode(buf)?;
        Ok(match message_type {
            ZksMessageId::GetBlockReplays => Self::GetBlockReplays(GetBlockReplays::decode(buf)?),
            ZksMessageId::BlockReplays => {
                Self::BlockReplays(BlockReplays::<P::Record>::decode(buf)?)
            }
        })
    }
}

impl<P: ZksProtocolVersionSpec> Encodable for ZksMessage<P> {
    fn encode(&self, out: &mut dyn BufMut) {
        self.message_id().encode(out);
        match self {
            ZksMessage::GetBlockReplays(message) => message.encode(out),
            ZksMessage::BlockReplays(message) => message.encode(out),
        }
    }

    fn length(&self) -> usize {
        self.message_id().length()
            + match self {
                ZksMessage::GetBlockReplays(message) => message.length(),
                ZksMessage::BlockReplays(message) => message.length(),
            }
    }
}

/// Represents message IDs for zks protocol messages.
///
/// IDs `0x02`-`0x06` were used by the verifier messages of the retired `zks/3`/`zks/4` versions
/// (they now live in the `zks_2fa` subprotocol). Do not reuse them without bumping the protocol
/// version.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZksMessageId {
    /// Get block replays message.
    GetBlockReplays = 0x00,
    /// Block replays message.
    BlockReplays = 0x01,
}

impl ZksMessageId {
    /// Returns the corresponding `u8` value for a `ZksMessageId`.
    pub const fn as_u8(&self) -> u8 {
        *self as u8
    }
}

impl Encodable for ZksMessageId {
    fn encode(&self, out: &mut dyn BufMut) {
        out.put_u8(self.as_u8());
    }
    fn length(&self) -> usize {
        1
    }
}

impl Decodable for ZksMessageId {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        let byte = buf.first().ok_or(alloy_rlp::Error::InputTooShort)?;
        let id = ZksMessageId::try_from(*byte).map_err(RlpError::Custom)?;
        buf.advance(1);
        Ok(id)
    }
}

impl TryFrom<u8> for ZksMessageId {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::GetBlockReplays),
            0x01 => Ok(Self::BlockReplays),
            _ => Err("unrecognized zks message id"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ZksMessage;
    use crate::version::ZksProtocolV5;
    use crate::wire::replays::RecordOverride;
    use alloy::primitives::Bytes;

    #[test]
    fn v5_round_trips_replay_messages() {
        let messages = [
            ZksMessage::<ZksProtocolV5>::get_block_replays(42, None, vec![]),
            ZksMessage::<ZksProtocolV5>::get_block_replays(
                42,
                Some(64),
                vec![RecordOverride {
                    block_number: 42,
                    db_key: Bytes::from_static(b"key"),
                }],
            ),
            ZksMessage::<ZksProtocolV5>::block_replays(vec![]),
        ];

        for message in messages {
            let encoded = message.encoded();
            let mut slice = encoded.as_ref();
            let decoded = ZksMessage::<ZksProtocolV5>::decode_message(&mut slice).unwrap();
            assert_eq!(decoded.encoded(), encoded);
            assert!(slice.is_empty());
        }
    }

    #[test]
    fn rejects_retired_verifier_message_ids() {
        // IDs 0x02-0x06 belonged to the verifier messages of retired zks/3 and zks/4.
        for retired_id in 0x02..=0x06u8 {
            let buf = [retired_id];
            let mut slice = buf.as_ref();
            let err = ZksMessage::<ZksProtocolV5>::decode_message(&mut slice).unwrap_err();
            assert_eq!(err, alloy_rlp::Error::Custom("unrecognized zks message id"));
        }
    }
}
