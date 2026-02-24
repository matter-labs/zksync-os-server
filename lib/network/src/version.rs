//! Support for representing the version of the `zks` protocol

use crate::wire::message::ZksMessageId;
use crate::wire::replays::{WireReplayRecord, v0, v1};
use alloy::primitives::bytes::BufMut;
use alloy::rlp::{Decodable, Encodable, Error as RlpError};
use std::fmt::Debug;

/// Any protocol version along with its pinned wire formats.
pub trait AnyZksProtocolVersion: Debug + Send + Sync + Unpin + Clone + 'static {
    /// Wire format for replay record.
    type Record: WireReplayRecord;

    /// Version number matching this protocol version.
    const VERSION: ZksVersion;
}

/// Protocol version 0 is very bare-bones and used purely for testing.
#[derive(Debug, Clone)]
pub struct ZksProtocolV0;

impl AnyZksProtocolVersion for ZksProtocolV0 {
    type Record = v0::ReplayRecord;

    const VERSION: ZksVersion = ZksVersion::Zks0;
}

/// Protocol version 1 is the initial implementation that supports `GetBlockReplays` and `BlockReplays`
/// message types.
#[derive(Debug, Clone)]
pub struct ZksProtocolV1;

impl AnyZksProtocolVersion for ZksProtocolV1 {
    type Record = v1::ReplayRecord;

    const VERSION: ZksVersion = ZksVersion::Zks1;
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
}

impl ZksVersion {
    /// The latest known zks version
    pub const LATEST: Self = Self::Zks1;

    /// All known zks versions
    pub const ALL_VERSIONS: &'static [Self] = &[Self::Zks0, Self::Zks1];

    /// Returns the max message id for the given version.
    const fn max_message_id(&self) -> u8 {
        match self {
            ZksVersion::Zks0 => ZksMessageId::BlockReplays as u8,
            ZksVersion::Zks1 => ZksMessageId::BlockReplays as u8,
        }
    }

    /// Returns the total number of message types for the given version.
    pub(crate) const fn message_count(&self) -> u8 {
        self.max_message_id() + 1
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
/// use zksync_os_network::wire::ZksVersion;
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ZksVersion;
    use alloy::primitives::bytes::BytesMut;
    use alloy_rlp::{Decodable, Encodable, Error as RlpError};

    #[test]
    fn test_zks_version_rlp_encode() {
        // Version 0 is purposefully left out as it encodes to 0x80 (prefix for 0-length string)
        let versions = [ZksVersion::Zks1];

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
            (2_u8, Err(RlpError::Custom("invalid zks version"))),
        ];

        for (input, expected) in test_cases {
            let mut encoded = BytesMut::new();
            input.encode(&mut encoded);

            let mut slice = encoded.as_ref();
            let result = ZksVersion::decode(&mut slice);
            assert_eq!(result, expected);
        }
    }
}
