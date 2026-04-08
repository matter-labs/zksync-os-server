//! Support for representing the version of the `zks` protocol.

use crate::wire::message::ZksMessageId;
use crate::wire::replays::{WireReplayRecord, v0, v1, v2};
use alloy::primitives::bytes::BufMut;
use alloy::rlp::{Decodable, Encodable, Error as RlpError};
use std::fmt::Debug;

/// Type-level specification for a `zks` protocol version and its pinned wire formats.
pub trait ZksProtocolVersionSpec: Debug + Send + Sync + Unpin + Clone + 'static {
    /// Wire format for replay record.
    type Record: WireReplayRecord;

    /// Version number matching this protocol version.
    const VERSION: ZksVersion;
}

/// Protocol version 0 is very bare-bones and used purely for testing.
#[derive(Debug, Clone)]
pub struct ZksProtocolV0;

impl ZksProtocolVersionSpec for ZksProtocolV0 {
    type Record = v0::ReplayRecord;

    const VERSION: ZksVersion = ZksVersion::Zks0;
}

/// Protocol version 1 is the initial implementation that supports `GetBlockReplays` and `BlockReplays`
/// message types.
#[derive(Debug, Clone)]
pub struct ZksProtocolV1;

impl ZksProtocolVersionSpec for ZksProtocolV1 {
    type Record = v1::ReplayRecord;

    const VERSION: ZksVersion = ZksVersion::Zks1;
}

/// Protocol version 2 keeps the replay transport from v1 but upgrades the replay record encoding.
#[derive(Debug, Clone)]
pub struct ZksProtocolV2;

impl ZksProtocolVersionSpec for ZksProtocolV2 {
    type Record = v2::ReplayRecord;

    const VERSION: ZksVersion = ZksVersion::Zks2;
}

/// Protocol version 3 keeps replay record encoding from v2 and adds verifier-related messages:
/// `VerifierRoleRequest`, `VerifierChallenge`, `VerifierAuth`, `VerifyBatch`, and
/// `VerifyBatchResult`.
#[derive(Debug, Clone)]
pub struct ZksProtocolV3;

impl ZksProtocolVersionSpec for ZksProtocolV3 {
    type Record = v2::ReplayRecord;

    const VERSION: ZksVersion = ZksVersion::Zks3;
}

/// Error thrown when failed to parse a valid [`ZksVersion`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Unknown zks protocol version: {0}")]
pub struct ParseVersionError(String);

/// The `zks` protocol version.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum ZksVersion {
    /// The `zks` protocol version 0. Only used for testing.
    Zks0 = 0,
    /// The `zks` protocol version 1.
    Zks1 = 1,
    /// The `zks` protocol version 2.
    Zks2 = 2,
    /// The `zks` protocol version 3.
    Zks3 = 3,
}

impl ZksVersion {
    /// The latest known zks version
    pub const LATEST: Self = Self::Zks3;

    /// All known zks versions
    pub const ALL_VERSIONS: &'static [Self] = &[Self::Zks0, Self::Zks1, Self::Zks2, Self::Zks3];

    /// Returns the max message id for the given version.
    const fn max_message_id(&self) -> u8 {
        match self {
            ZksVersion::Zks0 => ZksMessageId::BlockReplays as u8,
            ZksVersion::Zks1 => ZksMessageId::BlockReplays as u8,
            ZksVersion::Zks2 => ZksMessageId::BlockReplays as u8,
            ZksVersion::Zks3 => ZksMessageId::VerifyBatchResult as u8,
        }
    }

    /// Returns the total number of message types for the given version.
    pub(crate) const fn message_count(&self) -> u8 {
        self.max_message_id() + 1
    }

    /// Returns whether this version recognizes the given message ID.
    pub(crate) const fn supports_message(&self, message: ZksMessageId) -> bool {
        message.as_u8() <= self.max_message_id()
    }
}

/// RLP encodes `ZksVersion` as a single byte.
impl Encodable for ZksVersion {
    fn encode(&self, out: &mut dyn BufMut) {
        (*self as u8).encode(out)
    }

    fn length(&self) -> usize {
        (*self as u8).length()
    }
}

/// RLP decodes a single byte into `ZksVersion`.
/// Returns error if byte is not a valid version.
impl Decodable for ZksVersion {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let version = u8::decode(buf)?;
        Self::try_from(version).map_err(|_| RlpError::Custom("invalid zks version"))
    }
}

/// Allow for converting from a u8 to an `ZksVersion`.
///
/// # Example
/// ```
/// use zksync_os_network::version::ZksVersion;
///
/// let version = ZksVersion::try_from(1).unwrap();
/// assert_eq!(version, ZksVersion::Zks1);
/// ```
impl TryFrom<u8> for ZksVersion {
    type Error = ParseVersionError;

    #[inline]
    fn try_from(u: u8) -> Result<Self, Self::Error> {
        match u {
            0 => Ok(Self::Zks0),
            1 => Ok(Self::Zks1),
            2 => Ok(Self::Zks2),
            3 => Ok(Self::Zks3),
            _ => Err(ParseVersionError(u.to_string())),
        }
    }
}

impl From<ZksVersion> for u8 {
    #[inline]
    fn from(v: ZksVersion) -> Self {
        v as Self
    }
}

impl From<ZksVersion> for &'static str {
    #[inline]
    fn from(v: ZksVersion) -> &'static str {
        match v {
            ZksVersion::Zks0 => "0",
            ZksVersion::Zks1 => "1",
            ZksVersion::Zks2 => "2",
            ZksVersion::Zks3 => "3",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ZksVersion;
    use crate::wire::message::ZksMessageId;
    use alloy::primitives::bytes::BytesMut;
    use alloy_rlp::{Decodable, Encodable, Error as RlpError};

    #[test]
    fn test_zks_version_rlp_encode() {
        // Version 0 is purposefully left out as it encodes to 0x80 (prefix for 0-length string)
        let versions = [ZksVersion::Zks1, ZksVersion::Zks2, ZksVersion::Zks3];

        for version in versions {
            let mut encoded = BytesMut::new();
            version.encode(&mut encoded);

            assert_eq!(encoded.len(), 1);
            assert_eq!(encoded[0], version as u8);
        }
    }

    #[test]
    fn test_zks_version_rlp_decode() {
        let test_cases = [
            (0_u8, Ok(ZksVersion::Zks0)),
            (1_u8, Ok(ZksVersion::Zks1)),
            (2_u8, Ok(ZksVersion::Zks2)),
            (3_u8, Ok(ZksVersion::Zks3)),
            (4_u8, Err(RlpError::Custom("invalid zks version"))),
        ];

        for (input, expected) in test_cases {
            let mut encoded = BytesMut::new();
            input.encode(&mut encoded);

            let mut slice = encoded.as_ref();
            let result = ZksVersion::decode(&mut slice);
            assert_eq!(result, expected);
        }
    }

    #[test]
    fn test_message_counts_match_protocol_surface() {
        let test_cases = [
            (ZksVersion::Zks0, 2),
            (ZksVersion::Zks1, 2),
            (ZksVersion::Zks2, 2),
            (ZksVersion::Zks3, 7),
        ];

        for (version, expected_count) in test_cases {
            assert_eq!(version.message_count(), expected_count);
        }
    }

    #[test]
    fn test_supports_message_matches_version_capabilities() {
        let old_messages = [ZksMessageId::GetBlockReplays, ZksMessageId::BlockReplays];
        let new_messages = [
            ZksMessageId::VerifierRoleRequest,
            ZksMessageId::VerifierChallenge,
            ZksMessageId::VerifierAuth,
            ZksMessageId::VerifyBatch,
            ZksMessageId::VerifyBatchResult,
        ];

        for version in [ZksVersion::Zks0, ZksVersion::Zks1, ZksVersion::Zks2] {
            for message in old_messages {
                assert!(version.supports_message(message));
            }
            for message in new_messages {
                assert!(!version.supports_message(message));
            }
        }

        for message in old_messages.into_iter().chain(new_messages) {
            assert!(ZksVersion::Zks3.supports_message(message));
        }
    }
}
