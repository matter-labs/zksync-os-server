//! Defines structs/enums for messages and request-response pairs used in zks wire protocol.
//! Handles compatibility with [`ZksVersion`].
//!
//! Examples include creating, encoding, and decoding protocol messages.

use crate::version::{AnyZksProtocolVersion, ZksVersion};
use crate::wire::replays::RecordOverride;
use crate::wire::{BlockReplays, GetBlockReplays};
use alloy::primitives::BlockNumber;
use alloy::primitives::bytes::{Buf, BufMut, BytesMut};
use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use reth_eth_wire::protocol::Protocol;
use reth_network::types::Capability;
use std::fmt::Debug;
use zksync_os_storage_api::ReplayRecord;

pub const ZKS_PROTOCOL: &str = "zks";

/// Represents a message in the zks wire protocol, versions 1-1.
///
/// As of version 1, the only supported method of communication is streaming: at least one side of
/// the connection MUST send exactly one [`GetBlockReplays`] request at the start of connection. Main
/// node (block producer) MUST NOT send the request but if both participants are external nodes then
/// both sides MAY send the request. The rest of the connection MUST consist of indefinite number of
/// [`BlockReplays`] messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZksMessage<P: AnyZksProtocolVersion> {
    /// Represents a `GetBlockReplays` streaming request.
    GetBlockReplays(GetBlockReplays),
    /// Represents a `BlockReplays` streaming response (one of many).
    BlockReplays(BlockReplays<P::Record>),
}

impl<P: AnyZksProtocolVersion> ZksMessage<P> {
    /// Returns the capability for the zks protocol.
    pub const fn capability() -> Capability {
        Capability::new_static(ZKS_PROTOCOL, P::VERSION as usize)
    }

    /// Returns the protocol for the zks protocol.
    pub const fn protocol() -> Protocol {
        Protocol::new(Self::capability(), ZksMessageId::message_count(P::VERSION))
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
        record_overrides: Vec<RecordOverride>,
    ) -> Self {
        Self::GetBlockReplays(GetBlockReplays {
            starting_block,
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

impl<P: AnyZksProtocolVersion> Encodable for ZksMessage<P> {
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
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZksMessageId {
    /// Get block replays message.
    GetBlockReplays = 0x00,
    /// Block replays message.
    BlockReplays = 0x01,
    // todo: consider `Other` variant
}

impl ZksMessageId {
    /// Returns the corresponding `u8` value for a `ZksMessageId`.
    pub const fn to_u8(&self) -> u8 {
        *self as u8
    }

    /// Returns the max value for the given version.
    pub const fn max(version: ZksVersion) -> u8 {
        match version {
            ZksVersion::Zks0 => Self::BlockReplays as u8,
            ZksVersion::Zks1 => Self::BlockReplays as u8,
        }
    }

    /// Returns the total number of message types for the given version.
    pub const fn message_count(version: ZksVersion) -> u8 {
        Self::max(version) + 1
    }
}

impl Encodable for ZksMessageId {
    fn encode(&self, out: &mut dyn BufMut) {
        out.put_u8(self.to_u8());
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
