use num_enum::TryFromPrimitive;

use super::ProtocolSemanticVersion;

/// Identifier of the proving harness that must be used to generate and verify proofs for a given execution version.
/// Unlike `ExecutionVersion`, this may change in _each_ protocol version, e.g. in patches.
/// The main difference is that even if the state transition function remains the same,
/// there might be changes in the proving circuit which would not change the outcome of execution,
/// but would require different proving and verification keys.
#[derive(Debug, Clone, Copy, TryFromPrimitive, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ProvingVersion {
    V6 = 6,
    V7 = 7,
    V8 = 8,
}

impl TryFrom<ProtocolSemanticVersion> for ProvingVersion {
    type Error = ProvingVersionError;

    fn try_from(version: ProtocolSemanticVersion) -> Result<Self, Self::Error> {
        match (version.major, version.minor, version.patch) {
            (0, 30, 1) | (0, 30, 2) => Ok(ProvingVersion::V6),
            (0, 31, 0) | (0, 31, 1) => Ok(ProvingVersion::V7),
            (0, 32, 0) => Ok(ProvingVersion::V8),
            _ => Err(ProvingVersionError::UnsupportedVersion(version)),
        }
    }
}

impl ProvingVersion {
    /// verification key hash generated from zksync-os v0.2.5, zksync-airbender v0.5.2 and zkos-wrapper v0.5.4
    const V6_VK_HASH: &'static str =
        "0x124ebcd537a1e1c152774dd18f67660e35625bba0b669bf3b4836d636b105337";

    /// verification key hash generated from zksync-os v0.3.0, zksync-airbender v0.5.2 and zkos-wrapper v0.5.5
    const V7_VK_HASH: &'static str =
        "0x23156cf220288cd1e436dccfc09aa4883ea8288da61aa69e2c7251b0c0c44ccd";

    /// verification key hash generated from zksync-airbender v0.6.0-rc.2 and zkos-wrapper
    /// v0.6.0-rc.2; matches the V8 entry in zksync-airbender-prover.
    /// App-SPECIFIC: the SNARK wrapper runs with `check_aux_params`, constraining the FRI
    /// proof's registers 18..=25 to the app program's commitment in-circuit, so the VK
    /// binds `multiblock_batch.bin` (md5 31cb9cb3b42d4a183fb858594eeb8706, built from the
    /// zksync-os v0.4.0 release tag) and must be regenerated whenever that binary changes.
    /// **100-bit security**: the level selects the `*_security_100_bits` recursion verifier
    /// binaries and so changes the recursion chain; the 80-bit hash for the same binary is a
    /// different value and is not interchangeable with this one.
    const V8_VK_HASH: &'static str =
        "0x9f7576b911e7d3f528d49f894208682c81800814db9e3beac7fc3b1c4d626e7a";

    /// Get the verification key hash associated with this execution version.
    pub fn vk_hash(&self) -> &'static str {
        match self {
            Self::V6 => Self::V6_VK_HASH,
            Self::V7 => Self::V7_VK_HASH,
            Self::V8 => Self::V8_VK_HASH,
        }
    }

    /// Try to get ExecutionVersion from verification key hash.
    pub fn try_from_vk_hash(vk_hash: &str) -> Result<Self, ProvingVersionError> {
        match vk_hash {
            Self::V6_VK_HASH => Ok(Self::V6),
            Self::V7_VK_HASH => Ok(Self::V7),
            Self::V8_VK_HASH => Ok(Self::V8),
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
        let test_vector = [
            ((0, 30, 1), ProvingVersion::V6),
            ((0, 30, 2), ProvingVersion::V6),
            ((0, 31, 0), ProvingVersion::V7),
            ((0, 31, 1), ProvingVersion::V7),
            ((0, 32, 0), ProvingVersion::V8),
        ];

        for ((major, minor, patch), expected) in test_vector.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            let proving_version = ProvingVersion::try_from(version.clone())
                .unwrap_or_else(|e| panic!("Failed to convert version {version:?}: {e}"));
            assert_eq!(&proving_version, expected);
        }

        let unknown_versions = [
            (0, 29, 1),
            (0, 30, 0),
            (0, 30, 3),
            (0, 32, 1),
            (0, 33, 0),
            (1, 30, 1),
            (1, 31, 0),
        ];

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
            (ProvingVersion::V6, ProvingVersion::V6_VK_HASH),
            (ProvingVersion::V7, ProvingVersion::V7_VK_HASH),
            (ProvingVersion::V8, ProvingVersion::V8_VK_HASH),
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
