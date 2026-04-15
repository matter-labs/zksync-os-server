//! Definitions for `zks` wire-protocol messages and version-aware encode / decode helpers.

use crate::version::ZksProtocolVersionSpec;
use crate::wire::{
    BlockReplays, GetBlockReplays,
    auth::{VerifierAuth, VerifierChallenge, VerifierRoleRequest},
    replays::RecordOverride,
    verification::{VerifyBatch, VerifyBatchResult},
};
use alloy::primitives::{
    B256, BlockNumber,
    bytes::{Buf, BufMut, BytesMut},
};
use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use reth_eth_wire::protocol::Protocol;
use reth_network::types::Capability;
use std::fmt::Debug;
use zksync_os_storage_api::ReplayRecord;

pub const ZKS_PROTOCOL: &str = "zks";

/// A `zks` wire-protocol message.
///
/// This enum is the union of all message types supported across `zks` protocol versions.
/// Individual versions advertise and decode only the subset of messages they support.
///
/// Versions `zks/0`, `zks/1`, and `zks/2` support replay streaming only via
/// [`GetBlockReplays`] and [`BlockReplays`]. Version `zks/3` keeps that replay transport and
/// adds verifier authentication plus batch verification request / response messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZksMessage<P: ZksProtocolVersionSpec> {
    /// Represents a `GetBlockReplays` streaming request.
    GetBlockReplays(GetBlockReplays),
    /// Represents a `BlockReplays` streaming response (one of many).
    BlockReplays(BlockReplays<P::Record>),
    /// External node requests verifier role for the current session.
    VerifierRoleRequest(VerifierRoleRequest),
    /// Main-node provides verifier challenge.
    VerifierChallenge(VerifierChallenge),
    /// External node authentication response proving control of the verifier signing key.
    VerifierAuth(VerifierAuth),
    /// Main node requests an external-node verifier to validate and sign a batch.
    VerifyBatch(VerifyBatch),
    /// External-node verifier responds to a [`VerifyBatch`] request with approval or refusal.
    VerifyBatchResult(VerifyBatchResult),
}

impl<P: ZksProtocolVersionSpec> ZksMessage<P> {
    /// Returns the capability for the zks protocol.
    pub const fn capability() -> Capability {
        Capability::new_static(ZKS_PROTOCOL, P::VERSION as usize)
    }

    /// Returns the protocol for the zks protocol.
    pub const fn protocol() -> Protocol {
        Protocol::new(Self::capability(), P::VERSION.message_count())
    }

    /// Returns the message's ID.
    pub const fn message_id(&self) -> ZksMessageId {
        match self {
            ZksMessage::GetBlockReplays(_) => ZksMessageId::GetBlockReplays,
            ZksMessage::BlockReplays(_) => ZksMessageId::BlockReplays,
            ZksMessage::VerifierRoleRequest(_) => ZksMessageId::VerifierRoleRequest,
            ZksMessage::VerifierChallenge(_) => ZksMessageId::VerifierChallenge,
            ZksMessage::VerifierAuth(_) => ZksMessageId::VerifierAuth,
            ZksMessage::VerifyBatch(_) => ZksMessageId::VerifyBatch,
            ZksMessage::VerifyBatchResult(_) => ZksMessageId::VerifyBatchResult,
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

    pub fn verifier_challenge(nonce: B256) -> Self {
        Self::VerifierChallenge(VerifierChallenge { nonce })
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
        if !P::VERSION.supports_message(message_type) {
            return Err(RlpError::Custom(
                "unsupported zks message id for protocol version",
            ));
        }
        Ok(match message_type {
            ZksMessageId::GetBlockReplays => Self::GetBlockReplays(GetBlockReplays::decode(buf)?),
            ZksMessageId::BlockReplays => {
                Self::BlockReplays(BlockReplays::<P::Record>::decode(buf)?)
            }
            ZksMessageId::VerifierRoleRequest => {
                Self::VerifierRoleRequest(VerifierRoleRequest::decode(buf)?)
            }
            ZksMessageId::VerifierChallenge => {
                Self::VerifierChallenge(VerifierChallenge::decode(buf)?)
            }
            ZksMessageId::VerifierAuth => Self::VerifierAuth(VerifierAuth::decode(buf)?),
            ZksMessageId::VerifyBatch => Self::VerifyBatch(VerifyBatch::decode(buf)?),
            ZksMessageId::VerifyBatchResult => {
                Self::VerifyBatchResult(VerifyBatchResult::decode(buf)?)
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
            ZksMessage::VerifierRoleRequest(message) => message.encode(out),
            ZksMessage::VerifierChallenge(message) => message.encode(out),
            ZksMessage::VerifierAuth(message) => message.encode(out),
            ZksMessage::VerifyBatch(message) => message.encode(out),
            ZksMessage::VerifyBatchResult(message) => message.encode(out),
        }
    }

    fn length(&self) -> usize {
        self.message_id().length()
            + match self {
                ZksMessage::GetBlockReplays(message) => message.length(),
                ZksMessage::BlockReplays(message) => message.length(),
                ZksMessage::VerifierRoleRequest(message) => message.length(),
                ZksMessage::VerifierChallenge(message) => message.length(),
                ZksMessage::VerifierAuth(message) => message.length(),
                ZksMessage::VerifyBatch(message) => message.length(),
                ZksMessage::VerifyBatchResult(message) => message.length(),
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
    /// Request verifier role.
    VerifierRoleRequest = 0x02,
    /// Verifier challenge message.
    VerifierChallenge = 0x03,
    /// Verifier auth message.
    VerifierAuth = 0x04,
    /// Batch verification request.
    VerifyBatch = 0x05,
    /// Batch verification response.
    VerifyBatchResult = 0x06,
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
            0x02 => Ok(Self::VerifierRoleRequest),
            0x03 => Ok(Self::VerifierChallenge),
            0x04 => Ok(Self::VerifierAuth),
            0x05 => Ok(Self::VerifyBatch),
            0x06 => Ok(Self::VerifyBatchResult),
            _ => Err("unrecognized zks message id"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ZksMessage;
    use crate::version::{ZksProtocolV1, ZksProtocolV2, ZksProtocolV3};
    use crate::wire::auth::{VerifierAuth, VerifierChallenge, VerifierRoleRequest};
    use crate::wire::verification::{VerifyBatch, VerifyBatchOutcome, VerifyBatchResult};
    use alloy::primitives::{B256, Bytes};

    #[test]
    fn v1_round_trips_replay_messages() {
        let messages = [
            ZksMessage::<ZksProtocolV1>::get_block_replays(42, None, vec![]),
            ZksMessage::<ZksProtocolV1>::block_replays(vec![]),
        ];

        for message in messages {
            let encoded = message.encoded();
            let mut slice = encoded.as_ref();
            let decoded = ZksMessage::<ZksProtocolV1>::decode_message(&mut slice).unwrap();
            assert_eq!(decoded.encoded(), encoded);
            assert!(slice.is_empty());
        }
    }

    #[test]
    fn v3_round_trips_new_messages() {
        let messages = [
            ZksMessage::<ZksProtocolV3>::VerifierRoleRequest(VerifierRoleRequest {}),
            ZksMessage::<ZksProtocolV3>::VerifierChallenge(VerifierChallenge {
                nonce: B256::repeat_byte(0x11),
            }),
            ZksMessage::<ZksProtocolV3>::VerifierAuth(VerifierAuth {
                signature: Bytes::from(vec![7u8; 65]),
            }),
            ZksMessage::<ZksProtocolV3>::VerifyBatch(VerifyBatch {
                request_id: 41,
                batch_number: 7,
                first_block_number: 100,
                last_block_number: 120,
                pubdata_mode: 0,
                commit_data: Bytes::from_static(b"commit"),
                prev_commit_data: Bytes::from_static(b"prev"),
                execution_protocol_version: 31,
            }),
            ZksMessage::<ZksProtocolV3>::VerifyBatchResult(VerifyBatchResult {
                request_id: 41,
                batch_number: 7,
                result: VerifyBatchOutcome::Approved(Bytes::from(vec![9u8; 65])),
            }),
        ];

        for message in messages {
            let encoded = message.encoded();
            let mut slice = encoded.as_ref();
            let decoded = ZksMessage::<ZksProtocolV3>::decode_message(&mut slice).unwrap();
            assert_eq!(decoded.encoded(), encoded);
            assert!(slice.is_empty());
        }
    }

    #[test]
    fn old_versions_reject_new_message_ids() {
        let encoded =
            ZksMessage::<ZksProtocolV3>::verifier_challenge(B256::repeat_byte(0xAA)).encoded();
        let mut slice = encoded.as_ref();
        let err = ZksMessage::<ZksProtocolV1>::decode_message(&mut slice).unwrap_err();
        assert_eq!(
            err,
            alloy_rlp::Error::Custom("unsupported zks message id for protocol version")
        );

        let mut slice = encoded.as_ref();
        let err = ZksMessage::<ZksProtocolV2>::decode_message(&mut slice).unwrap_err();
        assert_eq!(
            err,
            alloy_rlp::Error::Custom("unsupported zks message id for protocol version")
        );
    }
}
