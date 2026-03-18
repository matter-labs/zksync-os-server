use num_enum::TryFromPrimitive;

use super::ProtocolSemanticVersion;

/// Identifier of the proving harness that must be used to generate and verify proofs for a given execution version.
/// Unlike `ExecutionVersion`, this may change in _each_ protocol version, e.g. in patches.
/// The main difference is that even if the state transition function remains the same,
/// there might be changes in the proving circuit which would not change the outcome of execution,
/// but would require different proving and verification keys.
#[derive(Debug, Clone, Copy, TryFromPrimitive, PartialEq)]
#[repr(u32)]
pub enum ProvingVersion {
    V1 = 1,
    V2 = 2,
    V3 = 3,
    V4 = 4,
    V5 = 5,
    V6 = 6,
    V7 = 7,
}

impl TryFrom<ProtocolSemanticVersion> for ProvingVersion {
    type Error = ProvingVersionError;

    fn try_from(version: ProtocolSemanticVersion) -> Result<Self, Self::Error> {
        // Prior to v30 release, updates happened without proper protocol upgrades, so it's
        // impossible to determine an early version by the protocol version alone. However,
        // the precise execution version is stored in the block context, so it can be loaded
        // from there.
        match (version.minor, version.patch) {
            (29, 0) | (29, 1) => Ok(ProvingVersion::V4),
            (30, 0) => Ok(ProvingVersion::V5),
            (30, 1) => Ok(ProvingVersion::V6),
            (30, 2) => Ok(ProvingVersion::V6),
            (31, 0) => Ok(ProvingVersion::V7),
            (31, 1) => Ok(ProvingVersion::V7),
            (32, 0) => Ok(ProvingVersion::V7),
            _ => Err(ProvingVersionError::UnsupportedVersion(version)),
        }
    }
}

impl ProvingVersion {
    // NOTE: V1 and V2 have a slight chance of being off as they've been backfilled.
    // If you find a divergence in what you expect and the actual value, most likely a bug.

    /// verification key hash generated from zksync-os v0.0.21, zksync-airbender v0.4.4 and zkos-wrapper v0.4.3
    const V1_VK_HASH: &'static str =
        "0x80a72fbdf9d6ab299fb5dfc2bcc807cfc7be38c9cfb0bc9b1ce6f9510fb110ea";
    /// verification key hash generated from zksync-os v0.0.25, zksync-airbender v0.4.5 and zkos-wrapper v0.4.6
    const V2_VK_HASH: &'static str =
        "0x83d49897775e6c1f1d7247ec228e18158e8e3accda545c604de4c44eee1a9845";
    /// verification key hash generated from zksync-os v0.0.26, zksync-airbender v0.5.0 and zkos-wrapper v0.5.0
    const V3_VK_HASH: &'static str =
        "0x6a4509801ec284b8921c63dc6aaba668a0d71382d87ae4095ffc2235154e9fa3";
    /// verification key hash generated from zksync-os v0.1.0, zksync-airbender v0.5.1 and zkos-wrapper v0.5.3
    const V4_VK_HASH: &'static str =
        "0xa385a997a63cc78e724451dca8b044b5ef29fcdc9d8b6ced33d9f58de531faa5";

    /// verification key hash generated from zksync-os v0.2.4, zksync-airbender v0.5.1 and zkos-wrapper v0.5.3
    const V5_VK_HASH: &'static str =
        "0x996b02b1d0420e997b4dc0d629a3a1bba93ed3185ac463f17b02ff83be139581";

    /// verification key hash generated from zksync-os v0.2.5, zksync-airbender v0.5.2 and zkos-wrapper v0.5.4
    const V6_VK_HASH: &'static str =
        "0x124ebcd537a1e1c152774dd18f67660e35625bba0b669bf3b4836d636b105337";

    /// verification key hash generated from zksync-os dev-20260311, zksync-airbender v0.5.2 and zkos-wrapper v0.5.4
    /// TODO: use proper zksync-os release once it's there
    const V7_VK_HASH: &'static str =
        "0x84e5659871dfede9c6419d586ca4e4c175dae2652109421fa1e5d11b48711422";

    /// Get the verification key hash associated with this execution version.
    pub fn vk_hash(&self) -> &'static str {
        match self {
            Self::V1 => Self::V1_VK_HASH,
            Self::V2 => Self::V2_VK_HASH,
            Self::V3 => Self::V3_VK_HASH,
            Self::V4 => Self::V4_VK_HASH,
            Self::V5 => Self::V5_VK_HASH,
            Self::V6 => Self::V6_VK_HASH,
            Self::V7 => Self::V7_VK_HASH,
        }
    }

    /// Try to get ExecutionVersion from verification key hash.
    pub fn try_from_vk_hash(vk_hash: &str) -> Result<Self, ProvingVersionError> {
        match vk_hash {
            Self::V1_VK_HASH => Ok(Self::V1),
            Self::V2_VK_HASH => Ok(Self::V2),
            Self::V3_VK_HASH => Ok(Self::V3),
            Self::V4_VK_HASH => Ok(Self::V4),
            Self::V5_VK_HASH => Ok(Self::V5),
            Self::V6_VK_HASH => Ok(Self::V6),
            Self::V7_VK_HASH => Ok(Self::V7),
            val => Err(ProvingVersionError::UnsupportedVkHash(val.to_string())),
        }
    }
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum ProvingVersionError {
    #[error("Protocol version does not correspond to a known proving version: {0}")]
    UnsupportedVersion(ProtocolSemanticVersion),
    #[error("Verification key hash does not correspond to a known proving version: {0}")]
    UnsupportedVkHash(String),
}

#[cfg(test)]
mod tests {
    use super::{ProvingVersion, ProvingVersionError};
    use crate::ProtocolSemanticVersion;

    #[test]
    fn version_mapping() {
        // When adding new versions here, make sure to also update `unknown_versions` so that it makes sure
        // that the (new) next protocol version is unknown.
        let test_vector = [
            ((0, 29, 0), ProvingVersion::V4),
            ((0, 29, 1), ProvingVersion::V4),
            ((0, 30, 0), ProvingVersion::V5),
            ((0, 30, 1), ProvingVersion::V6),
            ((0, 31, 0), ProvingVersion::V7),
            ((0, 31, 1), ProvingVersion::V7),
            ((0, 32, 0), ProvingVersion::V7),
        ];

        for ((major, minor, patch), expected) in test_vector.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            let proving_version = ProvingVersion::try_from(version.clone())
                .unwrap_or_else(|e| panic!("Failed to convert version {version:?}: {e}"));
            assert_eq!(&proving_version, expected);
        }

        let unknown_versions = [(0, 27, 10), (0, 28, 5), (0, 30, 3), (0, 33, 0)];

        for (major, minor, patch) in unknown_versions.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            let proving_version = ProvingVersion::try_from(version);
            assert!(matches!(
                proving_version,
                Err(ProvingVersionError::UnsupportedVersion(_))
            ));
        }
    }

    #[test]
    fn vk_hash_mapping() {
        let test_vector = [
            (ProvingVersion::V1, ProvingVersion::V1_VK_HASH),
            (ProvingVersion::V2, ProvingVersion::V2_VK_HASH),
            (ProvingVersion::V3, ProvingVersion::V3_VK_HASH),
            (ProvingVersion::V4, ProvingVersion::V4_VK_HASH),
            (ProvingVersion::V5, ProvingVersion::V5_VK_HASH),
            (ProvingVersion::V6, ProvingVersion::V6_VK_HASH),
        ];

        for (proving_version, expected_vk_hash) in test_vector.iter() {
            let vk_hash = proving_version.vk_hash();
            assert_eq!(vk_hash, *expected_vk_hash);

            let parsed_proving_version =
                ProvingVersion::try_from_vk_hash(vk_hash).unwrap_or_else(|e| {
                    panic!("Failed to convert vk_hash {vk_hash} back to proving version: {e}")
                });
            assert_eq!(&parsed_proving_version, proving_version);
        }

        let unknown_hash = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let proving_version = ProvingVersion::try_from_vk_hash(unknown_hash);
        assert!(matches!(
            proving_version,
            Err(ProvingVersionError::UnsupportedVkHash(_))
        ));
    }
}
