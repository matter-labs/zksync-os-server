//! Definitions for `zks_2fa` wire-protocol messages and encode / decode helpers.
//!
//! The `zks_2fa` subprotocol carries verifier authentication and batch-verification request /
//! response traffic that previously lived inside `zks/3` and `zks/4`. It reuses the same message
//! payload types (see [`crate::wire::auth`] and [`crate::wire::verification`]) but renumbers the
//! message ids starting from `0x00` because it is a fresh, single-version protocol.

use crate::wire::auth::{VerifierAuth, VerifierChallenge, VerifierRoleRequest};
use crate::wire::verification::{VerifyBatch, VerifyBatchResult};
use alloy::primitives::{
    B256,
    bytes::{Buf, BufMut, BytesMut},
};
use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use reth_eth_wire::protocol::Protocol;
use reth_network::types::Capability;

pub const ZKS_2FA_PROTOCOL: &str = "zks_2fa";
pub(crate) const ZKS_2FA_PROTOCOL_VERSION: usize = 1;
pub(crate) const ZKS_2FA_MESSAGE_COUNT: u8 = 5;

/// A `zks_2fa` wire-protocol message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Zks2faMessage {
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

impl Zks2faMessage {
    /// Returns the capability for the `zks_2fa` protocol.
    pub const fn capability() -> Capability {
        Capability::new_static(ZKS_2FA_PROTOCOL, ZKS_2FA_PROTOCOL_VERSION)
    }

    /// Returns the protocol for the `zks_2fa` protocol.
    pub const fn protocol() -> Protocol {
        Protocol::new(Self::capability(), ZKS_2FA_MESSAGE_COUNT)
    }

    /// Returns the message's ID.
    pub const fn message_id(&self) -> Zks2faMessageId {
        match self {
            Zks2faMessage::VerifierRoleRequest(_) => Zks2faMessageId::VerifierRoleRequest,
            Zks2faMessage::VerifierChallenge(_) => Zks2faMessageId::VerifierChallenge,
            Zks2faMessage::VerifierAuth(_) => Zks2faMessageId::VerifierAuth,
            Zks2faMessage::VerifyBatch(_) => Zks2faMessageId::VerifyBatch,
            Zks2faMessage::VerifyBatchResult(_) => Zks2faMessageId::VerifyBatchResult,
        }
    }

    pub fn verifier_role_request() -> Self {
        Self::VerifierRoleRequest(VerifierRoleRequest {})
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

    /// Decodes a `Zks2faMessage` from the given message buffer.
    pub fn decode_message(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let message_type = Zks2faMessageId::decode(buf)?;
        Ok(match message_type {
            Zks2faMessageId::VerifierRoleRequest => {
                Self::VerifierRoleRequest(VerifierRoleRequest::decode(buf)?)
            }
            Zks2faMessageId::VerifierChallenge => {
                Self::VerifierChallenge(VerifierChallenge::decode(buf)?)
            }
            Zks2faMessageId::VerifierAuth => Self::VerifierAuth(VerifierAuth::decode(buf)?),
            Zks2faMessageId::VerifyBatch => Self::VerifyBatch(VerifyBatch::decode(buf)?),
            Zks2faMessageId::VerifyBatchResult => {
                Self::VerifyBatchResult(VerifyBatchResult::decode(buf)?)
            }
        })
    }
}

impl Encodable for Zks2faMessage {
    fn encode(&self, out: &mut dyn BufMut) {
        self.message_id().encode(out);
        match self {
            Zks2faMessage::VerifierRoleRequest(message) => message.encode(out),
            Zks2faMessage::VerifierChallenge(message) => message.encode(out),
            Zks2faMessage::VerifierAuth(message) => message.encode(out),
            Zks2faMessage::VerifyBatch(message) => message.encode(out),
            Zks2faMessage::VerifyBatchResult(message) => message.encode(out),
        }
    }

    fn length(&self) -> usize {
        self.message_id().length()
            + match self {
                Zks2faMessage::VerifierRoleRequest(message) => message.length(),
                Zks2faMessage::VerifierChallenge(message) => message.length(),
                Zks2faMessage::VerifierAuth(message) => message.length(),
                Zks2faMessage::VerifyBatch(message) => message.length(),
                Zks2faMessage::VerifyBatchResult(message) => message.length(),
            }
    }
}

/// Represents message IDs for `zks_2fa` protocol messages.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zks2faMessageId {
    /// Request verifier role.
    VerifierRoleRequest = 0x00,
    /// Verifier challenge message.
    VerifierChallenge = 0x01,
    /// Verifier auth message.
    VerifierAuth = 0x02,
    /// Batch verification request.
    VerifyBatch = 0x03,
    /// Batch verification response.
    VerifyBatchResult = 0x04,
}

impl Zks2faMessageId {
    /// Returns the corresponding `u8` value for a `Zks2faMessageId`.
    pub const fn as_u8(&self) -> u8 {
        *self as u8
    }
}

impl Encodable for Zks2faMessageId {
    fn encode(&self, out: &mut dyn BufMut) {
        out.put_u8(self.as_u8());
    }
    fn length(&self) -> usize {
        1
    }
}

impl Decodable for Zks2faMessageId {
    fn decode(buf: &mut &[u8]) -> alloy::rlp::Result<Self> {
        let byte = buf.first().ok_or(alloy_rlp::Error::InputTooShort)?;
        let id = Zks2faMessageId::try_from(*byte).map_err(RlpError::Custom)?;
        buf.advance(1);
        Ok(id)
    }
}

impl TryFrom<u8> for Zks2faMessageId {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::VerifierRoleRequest),
            0x01 => Ok(Self::VerifierChallenge),
            0x02 => Ok(Self::VerifierAuth),
            0x03 => Ok(Self::VerifyBatch),
            0x04 => Ok(Self::VerifyBatchResult),
            _ => Err("unrecognized zks_2fa message id"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Zks2faMessage;
    use crate::wire::auth::{VerifierAuth, VerifierChallenge, VerifierRoleRequest};
    use crate::wire::verification::{VerifyBatch, VerifyBatchOutcome, VerifyBatchResult};
    use alloy::primitives::{B256, Bytes};

    #[test]
    fn round_trips_all_messages() {
        let messages = [
            Zks2faMessage::VerifierRoleRequest(VerifierRoleRequest {}),
            Zks2faMessage::VerifierChallenge(VerifierChallenge {
                nonce: B256::repeat_byte(0x11),
            }),
            Zks2faMessage::VerifierAuth(VerifierAuth {
                signature: Bytes::from(vec![7u8; 65]),
            }),
            Zks2faMessage::VerifyBatch(VerifyBatch {
                request_id: 41,
                batch_number: 7,
                first_block_number: 100,
                last_block_number: 120,
                pubdata_mode: 0,
                commit_data: Bytes::from_static(b"commit"),
                prev_commit_data: Bytes::from_static(b"prev"),
                execution_protocol_version: 31,
            }),
            Zks2faMessage::VerifyBatchResult(VerifyBatchResult {
                request_id: 41,
                batch_number: 7,
                result: VerifyBatchOutcome::Approved(Bytes::from(vec![9u8; 65])),
            }),
        ];

        for message in messages {
            let encoded = message.encoded();
            let mut slice = encoded.as_ref();
            let decoded = Zks2faMessage::decode_message(&mut slice).unwrap();
            assert_eq!(decoded, message);
            assert_eq!(decoded.encoded(), encoded);
            assert!(slice.is_empty());
        }
    }

    #[test]
    fn rejects_unknown_message_id() {
        let err = Zks2faMessage::decode_message(&mut [0x05u8].as_ref()).unwrap_err();
        assert_eq!(
            err,
            alloy_rlp::Error::Custom("unrecognized zks_2fa message id")
        );
    }
}
