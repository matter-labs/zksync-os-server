//! Support for representing the version of the `zks` protocol

use alloy::primitives::bytes::BufMut;
use alloy::rlp::{Decodable, Encodable, Error as RlpError};
use core::str::FromStr;
use serde::{Deserialize, Serialize};

/// Error thrown when failed to parse a valid [`ZksVersion`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Unknown zks protocol version: {0}")]
pub struct ParseVersionError(String);

/// The `zks` protocol version.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

/// Allow for converting from a `&str` to an `ZksVersion`.
///
/// # Example
/// ```
/// use zksync_os_network::wire::ZksVersion;
///
/// let version = ZksVersion::try_from("1").unwrap();
/// assert_eq!(version, ZksVersion::Zks1);
/// ```
impl TryFrom<&str> for ZksVersion {
    type Error = ParseVersionError;

    #[inline]
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "1" => Ok(Self::Zks1),
            _ => Err(ParseVersionError(s.to_string())),
        }
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
            1 => Ok(Self::Zks1),
            _ => Err(ParseVersionError(u.to_string())),
        }
    }
}

impl FromStr for ZksVersion {
    type Err = ParseVersionError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
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
    use super::{ParseVersionError, ZksVersion};
    use alloy::primitives::bytes::BytesMut;
    use alloy_rlp::{Decodable, Encodable, Error as RlpError};

    #[test]
    fn test_zks_version_try_from_str() {
        assert_eq!(ZksVersion::Zks0, ZksVersion::try_from("0").unwrap());
        assert_eq!(ZksVersion::Zks1, ZksVersion::try_from("1").unwrap());
        assert_eq!(
            Err(ParseVersionError("2".to_string())),
            ZksVersion::try_from("2")
        );
    }

    #[test]
    fn test_zks_version_from_str() {
        assert_eq!(ZksVersion::Zks0, "0".parse().unwrap());
        assert_eq!(ZksVersion::Zks1, "1".parse().unwrap());
        assert_eq!(
            Err(ParseVersionError("2".to_string())),
            "2".parse::<ZksVersion>()
        );
    }

    #[test]
    fn test_zks_version_rlp_encode() {
        let versions = [ZksVersion::Zks0, ZksVersion::Zks1];

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
