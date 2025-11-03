use std::fmt;

use alloy::primitives::U256;

pub const PACKED_SEMVER_MINOR_OFFSET: u32 = 32;
pub const PACKED_SEMVER_MINOR_MASK: u32 = 0xFFFF;
pub const PACKED_SEMVER_PATCH_MASK: u32 = 0xFFFFFFFF;

/// `ProtocolVersionId` is a unique identifier of the protocol version.
///
/// Note, that it is an identifier of the `minor` semver version of the protocol, with
/// the `major` version being `0`. Also, the protocol version on the contracts may contain
/// potential patch versions, that may have different contract behavior (e.g. Verifier), but it should not
/// impact the users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolSemanticVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Default for ProtocolSemanticVersion {
    fn default() -> Self {
        Self::latest()
    }
}

impl ProtocolSemanticVersion {
    pub const fn latest() -> Self {
        Self {
            major: 0,
            minor: 29,
            patch: 0,
        }
    }
}

impl fmt::Display for ProtocolSemanticVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
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

        Ok(Self {
            major: 0, // Always 0 per convention
            minor,
            patch,
        })
    }
}
