use super::ProtocolSemanticVersion;

/// Look up the execution version (u32) for a given protocol version.
/// This value is stored in `BlockContext.execution_version` and used by multivm
/// to dispatch to the correct forward_system crate.
pub fn execution_version(version: &ProtocolSemanticVersion) -> Result<u32, ProtocolConfigError> {
    // NOTE: the _next_ anticipated version MUST route to the current version, so that we can
    // test upgrade logic. Once you add a new version here, make sure that you add +1 version
    // and route it to the current latest version.
    match version.minor {
        29 => Ok(4),
        30 => Ok(5),
        31 => Ok(6),
        32 => Ok(6),
        _ => Err(ProtocolConfigError::UnsupportedVersion(version.clone())),
    }
}

/// Look up the verification key hash for a given protocol version.
pub fn vk_hash(version: &ProtocolSemanticVersion) -> Result<&'static str, ProtocolConfigError> {
    match (version.minor, version.patch) {
        (29, 0) | (29, 1) => Ok(VK_HASH_V4),
        (30, 0) => Ok(VK_HASH_V5),
        (30, 1) | (30, 2) => Ok(VK_HASH_V6),
        (31, 0) | (31, 1) => Ok(VK_HASH_V7),
        (32, 0) => Ok(VK_HASH_V7),
        _ => Err(ProtocolConfigError::UnsupportedVersion(version.clone())),
    }
}

/// Look up the proving version ID (u32) for a given protocol version.
/// This is used in serialized proof wire formats for backward compatibility.
pub fn proving_version_id(version: &ProtocolSemanticVersion) -> Result<u32, ProtocolConfigError> {
    match (version.minor, version.patch) {
        (29, 0) | (29, 1) => Ok(4),
        (30, 0) => Ok(5),
        (30, 1) | (30, 2) => Ok(6),
        (31, 0) | (31, 1) => Ok(7),
        (32, 0) => Ok(7),
        _ => Err(ProtocolConfigError::UnsupportedVersion(version.clone())),
    }
}

/// Verify that a VK hash is known and matches the expected VK hash for the given protocol version.
pub fn verify_vk_hash(
    version: &ProtocolSemanticVersion,
    submitted_vk_hash: &str,
) -> Result<(), ProtocolConfigError> {
    let expected = vk_hash(version)?;
    if expected == submitted_vk_hash {
        Ok(())
    } else {
        Err(ProtocolConfigError::VkHashMismatch {
            expected: expected.to_string(),
            actual: submitted_vk_hash.to_string(),
        })
    }
}

/// Check if a VK hash is known. Returns the VK hash back if valid.
pub fn validate_vk_hash(hash: &str) -> Result<&str, ProtocolConfigError> {
    match hash {
        VK_HASH_V1 | VK_HASH_V2 | VK_HASH_V3 | VK_HASH_V4 | VK_HASH_V5 | VK_HASH_V6 => Ok(hash),
        _ => Err(ProtocolConfigError::UnsupportedVkHash(hash.to_string())),
    }
}

// VK hash constants — verification key hashes for L1 proof verification.

/// verification key hash generated from zksync-os v0.0.21, zksync-airbender v0.4.4 and zkos-wrapper v0.4.3
const VK_HASH_V1: &str = "0x80a72fbdf9d6ab299fb5dfc2bcc807cfc7be38c9cfb0bc9b1ce6f9510fb110ea";
/// verification key hash generated from zksync-os v0.0.25, zksync-airbender v0.4.5 and zkos-wrapper v0.4.6
const VK_HASH_V2: &str = "0x83d49897775e6c1f1d7247ec228e18158e8e3accda545c604de4c44eee1a9845";
/// verification key hash generated from zksync-os v0.0.26, zksync-airbender v0.5.0 and zkos-wrapper v0.5.0
const VK_HASH_V3: &str = "0x6a4509801ec284b8921c63dc6aaba668a0d71382d87ae4095ffc2235154e9fa3";
/// verification key hash generated from zksync-os v0.1.0, zksync-airbender v0.5.1 and zkos-wrapper v0.5.3
const VK_HASH_V4: &str = "0xa385a997a63cc78e724451dca8b044b5ef29fcdc9d8b6ced33d9f58de531faa5";
/// verification key hash generated from zksync-os v0.2.4, zksync-airbender v0.5.1 and zkos-wrapper v0.5.3
const VK_HASH_V5: &str = "0x996b02b1d0420e997b4dc0d629a3a1bba93ed3185ac463f17b02ff83be139581";
/// verification key hash generated from zksync-os v0.2.5, zksync-airbender v0.5.2 and zkos-wrapper v0.5.4
const VK_HASH_V6: &str = "0x124ebcd537a1e1c152774dd18f67660e35625bba0b669bf3b4836d636b105337";
/// TODO: replace with the actual V7 VK hash once the proving circuit for v31 is finalized.
const VK_HASH_V7: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

#[derive(thiserror::Error, Debug, Clone)]
pub enum ProtocolConfigError {
    #[error("Protocol version does not have a known configuration: {0}")]
    UnsupportedVersion(ProtocolSemanticVersion),
    #[error("Verification key hash does not correspond to a known proving version: {0}")]
    UnsupportedVkHash(String),
    #[error("VK hash mismatch: expected {expected}, got {actual}")]
    VkHashMismatch { expected: String, actual: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProtocolSemanticVersion;

    #[test]
    fn execution_version_mapping() {
        let test_vector = [
            ((0, 29, 0), 4),
            ((0, 29, 1), 4),
            ((0, 30, 0), 5),
            ((0, 30, 1), 5),
            ((0, 31, 0), 6),
            ((0, 31, 1), 6),
            ((0, 32, 0), 6),
            ((0, 32, 1), 6),
        ];

        for ((major, minor, patch), expected) in test_vector.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            let exec_ver = execution_version(&version)
                .unwrap_or_else(|e| panic!("Failed to convert version {version:?}: {e}"));
            assert_eq!(exec_ver, *expected);
        }

        let unknown_versions = [(0, 27, 10), (0, 28, 5), (0, 33, 0)];
        for (major, minor, patch) in unknown_versions.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            assert!(execution_version(&version).is_err());
        }
    }

    #[test]
    fn proving_version_id_mapping() {
        let test_vector = [
            ((0, 29, 0), 4),
            ((0, 29, 1), 4),
            ((0, 30, 0), 5),
            ((0, 30, 1), 6),
            ((0, 30, 2), 6),
            ((0, 31, 0), 7),
            ((0, 31, 1), 7),
            ((0, 32, 0), 7),
        ];

        for ((major, minor, patch), expected) in test_vector.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            let pv_id = proving_version_id(&version)
                .unwrap_or_else(|e| panic!("Failed to convert version {version:?}: {e}"));
            assert_eq!(pv_id, *expected);
        }

        let unknown_versions = [(0, 27, 10), (0, 28, 5), (0, 30, 3), (0, 33, 0)];
        for (major, minor, patch) in unknown_versions.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            assert!(proving_version_id(&version).is_err());
        }
    }

    #[test]
    fn vk_hash_mapping() {
        let test_vector = [
            ((0, 29, 0), VK_HASH_V4),
            ((0, 30, 0), VK_HASH_V5),
            ((0, 30, 1), VK_HASH_V6),
            ((0, 31, 0), VK_HASH_V7),
        ];

        for ((major, minor, patch), expected_hash) in test_vector.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            let hash = vk_hash(&version)
                .unwrap_or_else(|e| panic!("Failed to get vk_hash for {version:?}: {e}"));
            assert_eq!(hash, *expected_hash);
        }
    }

    #[test]
    fn validate_known_vk_hashes() {
        assert!(validate_vk_hash(VK_HASH_V4).is_ok());
        assert!(validate_vk_hash(VK_HASH_V6).is_ok());
        assert!(
            validate_vk_hash("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
                .is_err()
        );
    }
}
