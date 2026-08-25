use super::ProtocolSemanticVersion;

/// Concrete implementation used to produce the prover input for a sealed batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProverInputStrategy {
    /// Generate per-block inputs with the zksync-os 0.2.x crates and combine them when sealing
    /// the batch.
    ZkOs0_2BlockInputs,
    /// Generate per-block inputs with the zksync-os 0.3.x crates and combine them when sealing
    /// the batch.
    ZkOs0_3BlockInputs,
    /// Preserve per-block tree data and re-execute the batch with the zksync-os 0.4.x multiblock
    /// batch program, which produces the batch prover input directly.
    ZkOs0_4NativeBatch,
}

impl ProverInputStrategy {
    pub const fn requires_native_batch_run(self) -> bool {
        match self {
            Self::ZkOs0_2BlockInputs | Self::ZkOs0_3BlockInputs => false,
            Self::ZkOs0_4NativeBatch => true,
        }
    }
}

/// FRI proof decoding, verification, and public-input construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriProofConfiguration {
    /// Decode a fully recursed airbender 0.5.x `ProgramProof`. Its public input omits the chain
    /// config hash, and the application is bound implicitly by the app-specific SNARK wrapper VK.
    ProgramProof,
    /// Decode an airbender 0.6.x `UnrolledProgramProof` recursed to the unified layer. Its public
    /// input includes the chain config hash, and since the unified-layer verifier is
    /// application-independent, the proof must be bound to `application_end_params` explicitly.
    UnrolledProof {
        application_end_params: &'static [u32; 8],
    },
}

/// Complete proving configuration for one protocol version.
/// Its address is the internal identity used for proof aggregation.
#[derive(Debug)]
pub struct ProvingConfiguration {
    pub protocol_version: ProtocolSemanticVersion,
    pub verification_key_hash: &'static str,
    pub prover_input: ProverInputStrategy,
    pub fri: FriProofConfiguration,
    /// Numeric selector encoded in the existing L1 proof calldata.
    pub l1_verifier_selector: u32,
}

#[derive(Debug)]
pub struct ProvingRegistry {
    entries: &'static [ProvingConfiguration],
}

impl ProvingRegistry {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &'static ProvingConfiguration> + Clone {
        self.entries.iter()
    }

    /// Looks up an exact semantic version. There is intentionally no range or latest-version
    /// fallback: an absent entry is unsupported for proving.
    pub fn get(
        &self,
        protocol_version: &ProtocolSemanticVersion,
    ) -> Option<&'static ProvingConfiguration> {
        self.entries
            .iter()
            .find(|config| &config.protocol_version == protocol_version)
    }

    pub fn require(
        &self,
        protocol_version: &ProtocolSemanticVersion,
        operation: &'static str,
    ) -> Result<&'static ProvingConfiguration, UnsupportedProtocolForProving> {
        self.get(protocol_version)
            .ok_or_else(|| UnsupportedProtocolForProving {
                protocol_version: protocol_version.clone(),
                operation,
            })
    }

    pub fn canonical_verification_key_hash(
        &self,
        verification_key_hash: &str,
    ) -> Option<&'static str> {
        self.entries.iter().find_map(|config| {
            let canonical_hash = config.verification_key_hash;
            (canonical_hash == verification_key_hash).then_some(canonical_hash)
        })
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error(
    "protocol version {protocol_version} is not registered for proving; required by {operation}"
)]
pub struct UnsupportedProtocolForProving {
    pub protocol_version: ProtocolSemanticVersion,
    pub operation: &'static str,
}

// The public v0.30.2 and v0.31.1 configurations duplicate proving artifacts so upgrade tests
// can cross these patch versions. Keep them as distinct entries: deployed configurations may
// use different verification keys and must not be grouped together.
static REGISTRY_ENTRIES: [ProvingConfiguration; 5] = [
    ProvingConfiguration {
        protocol_version: ProtocolSemanticVersion::new(0, 30, 1),
        // Generated from zksync-os v0.2.5, zksync-airbender v0.5.2, and
        // zkos-wrapper v0.5.4.
        verification_key_hash: "0x124ebcd537a1e1c152774dd18f67660e35625bba0b669bf3b4836d636b105337",
        prover_input: ProverInputStrategy::ZkOs0_2BlockInputs,
        fri: FriProofConfiguration::ProgramProof,
        l1_verifier_selector: 6,
    },
    ProvingConfiguration {
        protocol_version: ProtocolSemanticVersion::new(0, 30, 2),
        // Public upgrade tests reuse the v0.30.1 proving artifacts for this entry.
        verification_key_hash: "0x124ebcd537a1e1c152774dd18f67660e35625bba0b669bf3b4836d636b105337",
        prover_input: ProverInputStrategy::ZkOs0_2BlockInputs,
        fri: FriProofConfiguration::ProgramProof,
        l1_verifier_selector: 6,
    },
    ProvingConfiguration {
        protocol_version: ProtocolSemanticVersion::new(0, 31, 0),
        // Generated from zksync-os v0.3.0, zksync-airbender v0.5.2, and
        // zkos-wrapper v0.5.5.
        verification_key_hash: "0x23156cf220288cd1e436dccfc09aa4883ea8288da61aa69e2c7251b0c0c44ccd",
        prover_input: ProverInputStrategy::ZkOs0_3BlockInputs,
        fri: FriProofConfiguration::ProgramProof,
        // Selector 0 uses the L1 contract's default verifier for this configuration.
        l1_verifier_selector: 0,
    },
    ProvingConfiguration {
        protocol_version: ProtocolSemanticVersion::new(0, 31, 1),
        // Public upgrade tests reuse the v0.31.0 proving artifacts for this entry.
        verification_key_hash: "0x23156cf220288cd1e436dccfc09aa4883ea8288da61aa69e2c7251b0c0c44ccd",
        prover_input: ProverInputStrategy::ZkOs0_3BlockInputs,
        fri: FriProofConfiguration::ProgramProof,
        // Selector 0 uses the L1 contract's default verifier for this configuration.
        l1_verifier_selector: 0,
    },
    ProvingConfiguration {
        protocol_version: ProtocolSemanticVersion::new(0, 32, 0),
        // Generated from zksync-airbender v0.6.0-rc.2 and zkos-wrapper v0.6.0-rc.2.
        // The app-specific SNARK wrapper binds `multiblock_batch.bin` (md5
        // `31cb9cb3b42d4a183fb858594eeb8706`, built from zksync-os v0.4.0) through
        // `check_aux_params`. Its 100-bit recursion level is not interchangeable with the
        // 80-bit configuration for the same application.
        verification_key_hash: "0x9f7576b911e7d3f528d49f894208682c81800814db9e3beac7fc3b1c4d626e7a",
        prover_input: ProverInputStrategy::ZkOs0_4NativeBatch,
        fri: FriProofConfiguration::UnrolledProof {
            // `end_params` of the zksync-os v0.4.0 multiblock batch program at 69bc4305
            // (md5 `31cb9cb3b42d4a183fb858594eeb8706`) bind the application-independent
            // unified-layer verifier to this application.
            application_end_params: &[
                1634684069, 1321011044, 3947845475, 1282304698, 3895515656, 1824728812, 3916768926,
                1115552394,
            ],
        },
        l1_verifier_selector: 8,
    },
];

static REGISTRY: ProvingRegistry = ProvingRegistry {
    entries: &REGISTRY_ENTRIES,
};

pub fn proving_registry() -> &'static ProvingRegistry {
    &REGISTRY
}

pub fn require_proving_config(
    protocol_version: &ProtocolSemanticVersion,
    operation: &'static str,
) -> Result<&'static ProvingConfiguration, UnsupportedProtocolForProving> {
    proving_registry().require(protocol_version, operation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;

    #[test]
    fn production_registry_entries_are_valid() {
        let registry = proving_registry();
        let mut protocol_versions = std::collections::HashSet::new();

        for entry in registry.iter() {
            assert!(
                protocol_versions.insert(entry.protocol_version.clone()),
                "duplicate proving registry entry for protocol version {}",
                entry.protocol_version
            );
            assert!(
                entry.verification_key_hash.starts_with("0x"),
                "verification key hash for protocol version {} must start with 0x",
                entry.protocol_version
            );
            entry
                .verification_key_hash
                .parse::<B256>()
                .unwrap_or_else(|err| {
                    panic!(
                        "invalid verification key hash for protocol version {}: {err}",
                        entry.protocol_version
                    )
                });
            if let FriProofConfiguration::UnrolledProof {
                application_end_params,
            } = entry.fri
            {
                assert!(
                    application_end_params.iter().any(|word| *word != 0),
                    "application end parameters for protocol version {} are all zero",
                    entry.protocol_version
                );
            }
            crate::ExecutionVersion::try_from(&entry.protocol_version)
                .expect("registered proving protocol must have a known execution version");
        }
    }

    #[test]
    fn missing_exact_version_fails_closed() {
        static ENTRIES: [ProvingConfiguration; 1] = [ProvingConfiguration {
            protocol_version: ProtocolSemanticVersion::new(1, 0, 0),
            verification_key_hash: "0x23156cf220288cd1e436dccfc09aa4883ea8288da61aa69e2c7251b0c0c44ccd",
            prover_input: ProverInputStrategy::ZkOs0_3BlockInputs,
            fri: FriProofConfiguration::ProgramProof,
            l1_verifier_selector: 0,
        }];
        let registry = ProvingRegistry { entries: &ENTRIES };
        let missing = ProtocolSemanticVersion::new(1, 0, 1);
        let error = registry.require(&missing, "registry test").unwrap_err();
        assert_eq!(error.protocol_version, missing);
    }
}
