use num_enum::TryFromPrimitive;

use super::ProtocolSemanticVersion;

/// Identifier of the MultiVM execution version that corresponds to a concrete state transition function.
/// Generally this is depicted by the minor of the protocol version, e.g. it can (but not guaranteed to) only change
/// if the minor of the protocol version changes.
#[derive(Debug, Clone, Copy, TryFromPrimitive, PartialEq)]
#[repr(u32)]
pub enum ExecutionVersion {
    V1 = 1,
    V2 = 2,
    V3 = 3,
    V4 = 4,
    V5 = 5,
}

impl TryFrom<ProtocolSemanticVersion> for ExecutionVersion {
    type Error = ExecutionVersionError;

    fn try_from(version: ProtocolSemanticVersion) -> Result<Self, Self::Error> {
        // Prior to v30 release, updates happened without proper protocol upgrades, so it's
        // impossible to determine an early version by the protocol version alone. However,
        // the precise execution version is stored in the block context, so it can be loaded
        // from there.
        // NOTE: the _next_ anticipated version MUST route to the current version, so that we can
        // test upgrade logic. Once you add a new version here, make sure that you add +1 version
        // and route it to the current latest version.
        match version.minor {
            29 => Ok(ExecutionVersion::V4),
            30 => Ok(ExecutionVersion::V5),
            31 => Ok(ExecutionVersion::V5),
            _ => Err(ExecutionVersionError::UnsupportedVersion(version)),
        }
    }
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum ExecutionVersionError {
    #[error("Protocol version does not correspond to a known execution version: {0}")]
    UnsupportedVersion(ProtocolSemanticVersion),
}

#[cfg(test)]
mod tests {
    use super::{ExecutionVersion, ExecutionVersionError};
    use crate::ProtocolSemanticVersion;

    #[test]
    fn version_mapping() {
        // When adding new versions here, make sure to also update `unknown_versions` so that it makes sure
        // that the (new) next protocol version is unknown.
        let test_vector = [
            ((0, 29, 0), ExecutionVersion::V4),
            ((0, 29, 1), ExecutionVersion::V4),
            ((0, 30, 0), ExecutionVersion::V5),
            ((0, 30, 1), ExecutionVersion::V5),
            ((0, 31, 0), ExecutionVersion::V5),
            ((0, 31, 1), ExecutionVersion::V5),
        ];

        for ((major, minor, patch), expected) in test_vector.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            let exec_version = ExecutionVersion::try_from(version.clone())
                .unwrap_or_else(|e| panic!("Failed to convert version {version:?}: {e}"));
            assert_eq!(&exec_version, expected);
        }

        let unknown_versions = [(0, 27, 10), (0, 28, 5), (0, 32, 0)];

        for (major, minor, patch) in unknown_versions.iter() {
            let version = ProtocolSemanticVersion::new(*major, *minor, *patch);
            let exec_version = ExecutionVersion::try_from(version);
            assert!(matches!(
                exec_version,
                Err(ExecutionVersionError::UnsupportedVersion(_))
            ));
        }
    }
}
