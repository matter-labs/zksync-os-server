use alloy::primitives::{B256, keccak256};
use serde::Deserialize;
use std::sync::OnceLock;
use zksync_os_types::ProtocolSemanticVersion;

/// A released ZiSK proving stack known to this server binary.
///
/// Like Airbender's `ProvingVersion`, this is intentionally compiled into the
/// server: accepting a new guest, recursive setup, aggregator or L1 key is a
/// reviewed binary change, not an operator-local key override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ZiskProvingVersion {
    V1,
}

impl TryFrom<ProtocolSemanticVersion> for ZiskProvingVersion {
    type Error = ZiskProvingVersionError;

    fn try_from(version: ProtocolSemanticVersion) -> Result<Self, Self::Error> {
        match (version.major, version.minor, version.patch) {
            (0, 31, 0) | (0, 31, 1) => Ok(Self::V1),
            _ => Err(ZiskProvingVersionError::UnsupportedProtocolVersion(version)),
        }
    }
}

impl ZiskProvingVersion {
    pub const fn all() -> &'static [Self] {
        &[Self::V1]
    }

    pub fn protocol_versions(self) -> &'static [ProtocolSemanticVersion] {
        static V1: OnceLock<[ProtocolSemanticVersion; 2]> = OnceLock::new();
        match self {
            Self::V1 => V1.get_or_init(|| {
                [
                    ProtocolSemanticVersion::new(0, 31, 0),
                    ProtocolSemanticVersion::new(0, 31, 1),
                ]
            }),
        }
    }

    pub fn manifest(self) -> &'static ZiskReleaseManifest {
        static V1: OnceLock<ZiskReleaseManifest> = OnceLock::new();
        match self {
            Self::V1 => V1.get_or_init(|| {
                let manifest: ZiskReleaseManifest =
                    serde_json::from_str(include_str!("../manifests/zisk-proving-v1.json"))
                        .expect("the compiled ZiSK V1 release manifest must be valid JSON");
                manifest
                    .validate()
                    .expect("the compiled ZiSK V1 release manifest must be internally consistent");
                manifest
            }),
        }
    }

    pub fn keys(self) -> ZiskVersionKeys {
        self.manifest().keys()
    }

    pub fn verification_key_hash(self) -> B256 {
        self.manifest().zisk_verification_key_hash
    }

    pub fn try_from_vk_hash(hash: B256) -> Result<Self, ZiskProvingVersionError> {
        Self::all()
            .iter()
            .copied()
            .find(|version| version.verification_key_hash() == hash)
            .ok_or(ZiskProvingVersionError::UnsupportedVerificationKeyHash(
                hash,
            ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ZiskProvingVersionError {
    #[error("protocol version does not correspond to a known ZiSK proving version: {0}")]
    UnsupportedProtocolVersion(ProtocolSemanticVersion),
    #[error("verification key hash does not correspond to a known ZiSK proving version: {0}")]
    UnsupportedVerificationKeyHash(B256),
}

/// The four cryptographic identities the server needs on the proving path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZiskVersionKeys {
    pub verification_key_hash: B256,
    pub inner_program_vk: B256,
    pub aggregator_program_vk: B256,
    pub vadcop_vk: B256,
}

/// Canonical release metadata published by `zksync-os-zisk` and vendored into
/// this binary. CI consumes the same JSON file, so artifact selection and the
/// server's proof checks cannot drift independently.
#[derive(Debug, Deserialize)]
pub struct ZiskReleaseManifest {
    pub schema_version: u32,
    pub release: ZiskRelease,
    pub toolchain: ZiskToolchain,
    pub programs: ZiskPrograms,
    pub vadcop_final: ZiskVadcopFinal,
    pub artifacts: ZiskArtifacts,
    pub zisk_verification_key_hash: B256,
    pub zisk_verification_key_hash_preimage: [String; 3],
}

#[derive(Debug, Deserialize)]
pub struct ZiskRelease {
    pub repository: String,
    pub tag: String,
    pub commit: String,
}

#[derive(Debug, Deserialize)]
pub struct ZiskToolchain {
    pub zisk_version: String,
}

#[derive(Debug, Deserialize)]
pub struct ZiskPrograms {
    pub inner: ZiskProgram,
    pub aggregator: ZiskProgram,
}

#[derive(Debug, Deserialize)]
pub struct ZiskProgram {
    pub elf: ZiskArtifact,
    pub program_vk: B256,
}

#[derive(Debug, Deserialize)]
pub struct ZiskVadcopFinal {
    pub root_c: B256,
}

#[derive(Debug, Deserialize)]
pub struct ZiskArtifacts {
    pub guest_archive: ZiskArtifact,
    pub prover_archive: ZiskArtifact,
    pub prover_service: ZiskArtifact,
}

#[derive(Debug, Deserialize)]
pub struct ZiskArtifact {
    pub asset: String,
    pub sha256: String,
    pub size: u64,
}

impl ZiskReleaseManifest {
    pub fn keys(&self) -> ZiskVersionKeys {
        ZiskVersionKeys {
            verification_key_hash: self.zisk_verification_key_hash,
            inner_program_vk: self.programs.inner.program_vk,
            aggregator_program_vk: self.programs.aggregator.program_vk,
            vadcop_vk: self.vadcop_final.root_c,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.schema_version == 2, "unsupported ZiSK manifest schema");
        anyhow::ensure!(
            self.release.repository == "matter-labs/zksync-os-zisk",
            "unexpected ZiSK release repository"
        );
        anyhow::ensure!(
            self.release.commit.len() == 40
                && self
                    .release
                    .commit
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "ZiSK release commit must be a full lowercase Git commit ID"
        );
        anyhow::ensure!(!self.release.tag.is_empty(), "ZiSK release tag is empty");
        anyhow::ensure!(
            !self.toolchain.zisk_version.is_empty(),
            "ZiSK toolchain version is empty"
        );
        anyhow::ensure!(
            self.programs.inner.elf.asset == "zksync-os-zisk-guest"
                && self.programs.aggregator.elf.asset == "zksync-os-zisk-guest-aggregator"
                && self.artifacts.prover_service.asset == "zksync-os-zisk-prover-service",
            "ZiSK manifest uses unexpected binary names"
        );
        anyhow::ensure!(
            self.artifacts.guest_archive.asset
                == format!("zksync-os-zisk-guest-elfs-{}.tar.gz", self.release.tag)
                && self.artifacts.prover_archive.asset
                    == format!(
                        "zksync-os-zisk-prover-{}-x86_64-unknown-linux-gnu.tar.gz",
                        self.release.tag
                    ),
            "ZiSK release archive names do not match the release tag"
        );

        for artifact in [
            &self.programs.inner.elf,
            &self.programs.aggregator.elf,
            &self.artifacts.guest_archive,
            &self.artifacts.prover_archive,
            &self.artifacts.prover_service,
        ] {
            anyhow::ensure!(!artifact.asset.is_empty(), "ZiSK artifact name is empty");
            anyhow::ensure!(
                artifact.size > 0,
                "ZiSK artifact {} is empty",
                artifact.asset
            );
            anyhow::ensure!(
                artifact.sha256.len() == 64
                    && artifact
                        .sha256
                        .bytes()
                        .all(|byte| { byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) }),
                "ZiSK artifact {} has an invalid SHA-256 digest",
                artifact.asset
            );
        }

        anyhow::ensure!(
            self.zisk_verification_key_hash_preimage
                == [
                    "programs.inner.program_vk",
                    "programs.aggregator.program_vk",
                    "vadcop_final.root_c",
                ],
            "ZiSK verification-key hash preimage order changed"
        );
        let mut preimage = [0_u8; 96];
        preimage[..32].copy_from_slice(self.programs.inner.program_vk.as_slice());
        preimage[32..64].copy_from_slice(self.programs.aggregator.program_vk.as_slice());
        preimage[64..].copy_from_slice(self.vadcop_final.root_c.as_slice());
        anyhow::ensure!(
            keccak256(preimage) == self.zisk_verification_key_hash,
            "ZiSK verification-key hash does not bind the manifest key set"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    #[test]
    fn v31_protocol_versions_select_the_v1_manifest() {
        for version in [
            ProtocolSemanticVersion::new(0, 31, 0),
            ProtocolSemanticVersion::new(0, 31, 1),
        ] {
            assert_eq!(
                ZiskProvingVersion::try_from(version).unwrap(),
                ZiskProvingVersion::V1
            );
        }
        assert!(ZiskProvingVersion::try_from(ProtocolSemanticVersion::new(0, 32, 0)).is_err());
    }

    #[test]
    fn v1_manifest_binds_every_zisk_artifact_to_the_l1_identity() {
        let manifest = ZiskProvingVersion::V1.manifest();
        manifest.validate().unwrap();

        assert_eq!(manifest.release.tag, "0.0.3");
        assert_eq!(
            manifest.zisk_verification_key_hash,
            b256!("718bdb59530514f9a62f16b2ba912de17188615d82aa31ec681be4b9cd332888")
        );
        assert_eq!(
            manifest.artifacts.prover_service.sha256,
            "36e0af704fbb294658be338135b830fcf1e6b676affb04801d56573e35a58efc"
        );
    }
}
