use std::{fmt, ops::Deref, str::FromStr};

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

pub const PACKED_SEMVER_MINOR_OFFSET: u32 = 32;
pub const PACKED_SEMVER_MINOR_MASK: u32 = 0xFFFF;
pub const PACKED_SEMVER_PATCH_MASK: u32 = 0xFFFFFFFF;

/// `ProtocolVersionId` is a unique identifier of the protocol version.
///
/// Note, that it is an identifier of the `minor` semver version of the protocol, with
/// the `major` version being `0`. Also, the protocol version on the contracts may contain
/// potential patch versions, that may have different contract behavior (e.g. Verifier), but it should not
/// impact the users.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolSemanticVersion(semver::Version);

impl Default for ProtocolSemanticVersion {
    fn default() -> Self {
        Self::latest()
    }
}

// We allow accessing underlying semver, but we intentionally never want it to be modified.
impl Deref for ProtocolSemanticVersion {
    type Target = semver::Version;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ProtocolSemanticVersion {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self(semver::Version {
            major,
            minor,
            patch,
            pre: semver::Prerelease::EMPTY,
            build: semver::BuildMetadata::EMPTY,
        })
    }

    pub const fn latest() -> Self {
        Self::new(0, 29, 0)
    }
}

impl fmt::Display for ProtocolSemanticVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<U256> for ProtocolSemanticVersion {
    type Error = String;

    fn try_from(packed: U256) -> Result<Self, Self::Error> {
        let minor = ((packed >> U256::from(PACKED_SEMVER_MINOR_OFFSET))
            & U256::from(PACKED_SEMVER_MINOR_MASK))
        .try_into()
        .map_err(|err| format!("minor version overflow: {err}"))?;

        let patch = (packed & U256::from(PACKED_SEMVER_PATCH_MASK))
            .try_into()
            .map_err(|err| format!("patch version overflow: {err}"))?;

        Ok(Self::new(0, minor, patch))
    }
}

impl TryFrom<&str> for ProtocolSemanticVersion {
    type Error = semver::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let version = semver::Version::parse(value)?;
        Ok(Self(version))
    }
}

impl FromStr for ProtocolSemanticVersion {
    type Err = semver::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let version = semver::Version::parse(s)?;
        Ok(Self(version))
    }
}

#[cfg(test)]
mod tests {
    use super::ProtocolSemanticVersion;
    use alloy::primitives::U256;

    #[test]
    fn test_protocol_semantic_version_try_from_u256() {
        let packed = U256::from(0x0001_0000_0002u64);
        let version = ProtocolSemanticVersion::try_from(packed).unwrap();
        assert_eq!(version.major, 0);
        assert_eq!(version.minor, 1);
        assert_eq!(version.patch, 2);
    }

    #[test]
    fn test_protocol_semantic_version_display() {
        let version = ProtocolSemanticVersion::new(0, 29, 0);
        assert_eq!(version.to_string(), "0.29.0");
    }

    #[test]
    fn test_protocol_semantic_version_latest() {
        // Only change this test when you are sure it's safe to bump the latest protocol version.
        let latest = ProtocolSemanticVersion::latest();
        assert_eq!(latest.major, 0);
        assert_eq!(latest.minor, 29);
        assert_eq!(latest.patch, 0);
    }

    #[test]
    fn test_protocol_semantic_version_default() {
        let default = ProtocolSemanticVersion::default();
        assert_eq!(default, ProtocolSemanticVersion::latest());
    }

    #[test]
    fn test_protocol_semantiv_version_serde() {
        let version = ProtocolSemanticVersion::new(0, 29, 0);
        let serialized = serde_json::to_string(&version).unwrap();
        assert_eq!(serialized, r#""0.29.0""#);

        let deserialized: ProtocolSemanticVersion = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, version);
    }
}
