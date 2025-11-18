use std::{fmt, ops::Deref, str::FromStr};

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

mod execution_version;
mod proving_version;

pub use self::execution_version::{ExecutionVersion, ExecutionVersionError};
pub use self::proving_version::{ProvingVersion, ProvingVersionError};

const PACKED_SEMVER_PATCH_MASK: u32 = 0xFFFFFFFF;
const PACKED_SEMVER_MINOR_OFFSET: u32 = 32;
const PACKED_SEMVER_MINOR_MASK: u32 = 0xFFFFFFFF;
const PACKED_SEMVER_MAJOR_OFFSET: u32 = 64;

/// `ProtocolVersionId` is a unique identifier of the protocol version.
///
/// Note, that it is an identifier of the `minor` semver version of the protocol, with
/// the `major` version being `0`. Also, the protocol version on the contracts may contain
/// potential patch versions, that may have different contract behavior (e.g. Verifier), but it should not
/// impact the users.
// Default is not provided for `ProtocolSemanticVersion`, as it can cause issues in the decentralized network
// (imagine that EN will use it before executing the upgrade)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolSemanticVersion(semver::Version);

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

    /// Returns `true` if the system is live (or expected to be live) on any of the existing envs.
    /// Must be updated when a new version is ready to be released.
    pub fn is_live(&self) -> bool {
        if self.major != 0 {
            return false;
        }
        // Patch versions can always be live, as they don't change the state transition function.
        match self.minor {
            29 => true,
            // When updating this function, make sure to insert the new non-live version here.
            _ => false,
        }
    }

    /// This version was used for all the chains prior to the introduction of protocol upgrades
    /// support.
    pub const fn legacy_genesis_version() -> Self {
        Self::new(0, 29, 1)
    }

    /// Packs the semantic version into a `U256` according to the protocol encoding.
    /// Can return an error in case the stored version cannot be represented in the
    /// format expected by the protocol.
    pub fn packed(&self) -> Result<U256, ProtocolSemanticVersionError> {
        if self.major != 0 {
            return Err(ProtocolSemanticVersionError::MajorNonZero);
        }
        if self.minor > PACKED_SEMVER_MINOR_MASK as u64 {
            return Err(ProtocolSemanticVersionError::MinorOverflow);
        }
        if self.patch > PACKED_SEMVER_PATCH_MASK as u64 {
            return Err(ProtocolSemanticVersionError::PatchOverflow);
        }
        let minor = U256::from(self.minor) << PACKED_SEMVER_MINOR_OFFSET;
        let patch = U256::from(self.patch);
        Ok(minor | patch)
    }
}

#[derive(thiserror::Error, Debug, Clone, Copy)]
pub enum ProtocolSemanticVersionError {
    #[error("Minor version overflow")]
    MinorOverflow,
    #[error("Patch version overflow")]
    PatchOverflow,
    #[error("Major version must be 0")]
    MajorNonZero,
}

impl fmt::Display for ProtocolSemanticVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<U256> for ProtocolSemanticVersion {
    type Error = ProtocolSemanticVersionError;

    fn try_from(packed: U256) -> Result<Self, Self::Error> {
        let patch = (packed & U256::from(PACKED_SEMVER_PATCH_MASK))
            .try_into()
            .map_err(|_| ProtocolSemanticVersionError::PatchOverflow)?;

        let minor = ((packed >> U256::from(PACKED_SEMVER_MINOR_OFFSET))
            & U256::from(PACKED_SEMVER_MINOR_MASK))
        .try_into()
        .map_err(|_| ProtocolSemanticVersionError::MinorOverflow)?;

        let major = packed >> U256::from(PACKED_SEMVER_MAJOR_OFFSET);
        if major != U256::ZERO {
            return Err(ProtocolSemanticVersionError::MajorNonZero);
        }

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
    fn test_protocol_semantiv_version_serde() {
        let version = ProtocolSemanticVersion::new(0, 29, 0);
        let serialized = serde_json::to_string(&version).unwrap();
        assert_eq!(serialized, r#""0.29.0""#);

        let deserialized: ProtocolSemanticVersion = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, version);
    }

    #[test]
    fn test_protocol_semantic_version_is_live() {
        let test_vector = [
            ((0, 28, 5), false),
            ((0, 29, 0), true),
            ((0, 29, 1), true),
            ((0, 29, 99), true),
            ((0, 30, 0), false), // When updating this test, make sure to insert the new non-live version here.
        ];
        for ((major, minor, patch), expected) in test_vector.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            assert_eq!(version.is_live(), *expected);
        }
    }
}
