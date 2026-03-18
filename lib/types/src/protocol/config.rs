use super::ProtocolSemanticVersion;

// Include generated match-arm functions and VK hash list from protocol-versions.toml.
include!(concat!(env!("OUT_DIR"), "/protocol_config_generated.rs"));

/// Look up the execution version (u32) for a given protocol version.
/// This value is stored in `BlockContext.execution_version` and used by multivm
/// to dispatch to the correct forward_system crate.
pub fn execution_version(version: &ProtocolSemanticVersion) -> Result<u32, ProtocolConfigError> {
    execution_version_impl(version.minor, version.patch)
        .ok_or_else(|| ProtocolConfigError::UnsupportedVersion(version.clone()))
}

/// Look up the verification key hash for a given protocol version.
pub fn vk_hash(version: &ProtocolSemanticVersion) -> Result<&'static str, ProtocolConfigError> {
    vk_hash_impl(version.minor, version.patch)
        .ok_or_else(|| ProtocolConfigError::UnsupportedVersion(version.clone()))
}

/// Look up the proving version ID (u32) for a given protocol version.
/// This is used in serialized proof wire formats for backward compatibility.
pub fn proving_version_id(version: &ProtocolSemanticVersion) -> Result<u32, ProtocolConfigError> {
    proving_version_id_impl(version.minor, version.patch)
        .ok_or_else(|| ProtocolConfigError::UnsupportedVersion(version.clone()))
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
    if ALL_KNOWN_VK_HASHES.contains(&hash) {
        Ok(hash)
    } else {
        Err(ProtocolConfigError::UnsupportedVkHash(hash.to_string()))
    }
}

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
        let v29 = ProtocolSemanticVersion::new(0, 29, 0);
        let v30_0 = ProtocolSemanticVersion::new(0, 30, 0);
        let v30_1 = ProtocolSemanticVersion::new(0, 30, 1);
        let v31 = ProtocolSemanticVersion::new(0, 31, 0);

        let h29 = vk_hash(&v29).unwrap();
        let h30_0 = vk_hash(&v30_0).unwrap();
        let h30_1 = vk_hash(&v30_1).unwrap();
        let h31 = vk_hash(&v31).unwrap();

        // Different protocol versions should have different VK hashes (except 0.31 is placeholder).
        assert_ne!(h29, h30_0);
        assert_ne!(h30_0, h30_1);
        // All hashes should start with 0x.
        assert!(h29.starts_with("0x"));
        assert!(h30_0.starts_with("0x"));
        assert!(h30_1.starts_with("0x"));
        assert!(h31.starts_with("0x"));
    }

    #[test]
    fn validate_known_vk_hashes() {
        // VK hashes from active protocol versions should be valid.
        let v29_hash = vk_hash(&ProtocolSemanticVersion::new(0, 29, 0)).unwrap();
        assert!(validate_vk_hash(v29_hash).is_ok());

        let v30_1_hash = vk_hash(&ProtocolSemanticVersion::new(0, 30, 1)).unwrap();
        assert!(validate_vk_hash(v30_1_hash).is_ok());

        // Historical VK hashes should also be valid.
        assert!(
            validate_vk_hash("0x80a72fbdf9d6ab299fb5dfc2bcc807cfc7be38c9cfb0bc9b1ce6f9510fb110ea")
                .is_ok()
        );

        // Unknown hash should be invalid.
        assert!(
            validate_vk_hash("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
                .is_err()
        );
    }
}
