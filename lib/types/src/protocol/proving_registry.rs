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

/// Complete release-specific configuration for one reusable proving stack.
/// Registry entries that share a stack must reference the same static instance; its address is
/// the internal identity used for proof aggregation.
#[derive(Debug, Clone, Copy)]
pub struct ProvingStackConfiguration {
    /// A diagnostic name, not a serialized or independently numbered version.
    pub name: &'static str,
    pub verification_key_hash: &'static str,
    pub prover_input: ProverInputStrategy,
    pub fri: FriProofConfiguration,
    /// Index of the verifier this stack's SNARK proofs must be routed to in the L1 executor's
    /// verifier registry, encoded into `_proof[0]`. `0` means "the contract's default verifier".
    /// The numbering is fixed on-chain and inherited from the historical proving-version numbers,
    /// so it is unrelated to this stack's zksync-os release.
    pub l1_verifier_selector: u32,
}

#[derive(Debug, Clone)]
pub struct ProvingRegistryEntry {
    pub protocol_version: ProtocolSemanticVersion,
    pub configuration: &'static ProvingStackConfiguration,
}

#[derive(Debug)]
pub struct ProvingRegistry {
    entries: &'static [ProvingRegistryEntry],
}

impl ProvingRegistry {
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &'static ProvingRegistryEntry> + Clone {
        self.entries.iter()
    }

    /// Looks up an exact semantic version. There is intentionally no range or latest-version
    /// fallback: an absent entry is unsupported for proving.
    pub fn get(
        &self,
        protocol_version: &ProtocolSemanticVersion,
    ) -> Option<&'static ProvingStackConfiguration> {
        self.entries
            .iter()
            .find(|entry| &entry.protocol_version == protocol_version)
            .map(|entry| entry.configuration)
    }

    pub fn require(
        &self,
        protocol_version: &ProtocolSemanticVersion,
        operation: &'static str,
    ) -> Result<&'static ProvingStackConfiguration, UnsupportedProtocolForProving> {
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
        self.entries.iter().find_map(|entry| {
            let canonical_hash = entry.configuration.verification_key_hash;
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

/// Verification key hash generated from zksync-os v0.2.5, zksync-airbender v0.5.2 and
/// zkos-wrapper v0.5.4.
const ZK_OS_0_2_VK_HASH: &str =
    "0x124ebcd537a1e1c152774dd18f67660e35625bba0b669bf3b4836d636b105337";
/// Verification key hash generated from zksync-os v0.3.0, zksync-airbender v0.5.2 and
/// zkos-wrapper v0.5.5.
const ZK_OS_0_3_VK_HASH: &str =
    "0x23156cf220288cd1e436dccfc09aa4883ea8288da61aa69e2c7251b0c0c44ccd";
/// Verification key hash generated from zksync-airbender v0.6.0-rc.2 and zkos-wrapper
/// v0.6.0-rc.2; matches the 0.4.0 entry in zksync-airbender-prover.
/// App-SPECIFIC: the SNARK wrapper runs with `check_aux_params`, constraining the FRI
/// proof's registers 18..=25 to the app program's commitment in-circuit, so the VK
/// binds `multiblock_batch.bin` (md5 31cb9cb3b42d4a183fb858594eeb8706, built from the
/// zksync-os v0.4.0 release tag) and must be regenerated whenever that binary changes.
/// **100-bit security**: the level selects the `*_security_100_bits` recursion verifier
/// binaries and so changes the recursion chain; the 80-bit hash for the same binary is a
/// different value and is not interchangeable with this one.
const ZK_OS_0_4_VK_HASH: &str =
    "0x9f7576b911e7d3f528d49f894208682c81800814db9e3beac7fc3b1c4d626e7a";

static ZK_OS_0_2_PROVING_STACK: ProvingStackConfiguration = ProvingStackConfiguration {
    name: "zksync-os-0.2.x",
    verification_key_hash: ZK_OS_0_2_VK_HASH,
    prover_input: ProverInputStrategy::ZkOs0_2BlockInputs,
    fri: FriProofConfiguration::ProgramProof,
    l1_verifier_selector: 6,
};

static ZK_OS_0_3_PROVING_STACK: ProvingStackConfiguration = ProvingStackConfiguration {
    name: "zksync-os-0.3.x",
    verification_key_hash: ZK_OS_0_3_VK_HASH,
    prover_input: ProverInputStrategy::ZkOs0_3BlockInputs,
    fri: FriProofConfiguration::ProgramProof,
    // Selector 0 uses the L1 contract's default verifier, which is currently this stack's.
    l1_verifier_selector: 0,
};

/// `end_params` of the zksync-os 0.4.x multiblock batch program, built from the v0.4.0 release
/// tag @ 69bc4305 (md5 `31cb9cb3b42d4a183fb858594eeb8706`). The unified-layer FRI verifier is
/// application-independent, so this value binds its proof to the registered batch program.
const ZK_OS_0_4_APP_END_PARAMS: [u32; 8] = [
    1634684069, 1321011044, 3947845475, 1282304698, 3895515656, 1824728812, 3916768926, 1115552394,
];

static ZK_OS_0_4_PROVING_STACK: ProvingStackConfiguration = ProvingStackConfiguration {
    name: "zksync-os-0.4.x",
    verification_key_hash: ZK_OS_0_4_VK_HASH,
    prover_input: ProverInputStrategy::ZkOs0_4NativeBatch,
    fri: FriProofConfiguration::UnrolledProof {
        application_end_params: &ZK_OS_0_4_APP_END_PARAMS,
    },
    l1_verifier_selector: 8,
};

static REGISTRY_ENTRIES: [ProvingRegistryEntry; 5] = [
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 30, 1),
        configuration: &ZK_OS_0_2_PROVING_STACK,
    },
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 30, 2),
        configuration: &ZK_OS_0_2_PROVING_STACK,
    },
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 31, 0),
        configuration: &ZK_OS_0_3_PROVING_STACK,
    },
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 31, 1),
        configuration: &ZK_OS_0_3_PROVING_STACK,
    },
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 32, 0),
        configuration: &ZK_OS_0_4_PROVING_STACK,
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
) -> Result<&'static ProvingStackConfiguration, UnsupportedProtocolForProving> {
    proving_registry().require(protocol_version, operation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error, PartialEq, Eq)]
    enum ProvingRegistryInvariantError {
        #[error("duplicate proving registry entry for protocol version {0}")]
        DuplicateProtocolVersion(ProtocolSemanticVersion),
        #[error("proving stack `{0}` has an incomplete or invalid configuration: {1}")]
        InvalidConfiguration(&'static str, &'static str),
        #[error("proving stack name `{0}` identifies different configurations")]
        ConflictingStackName(&'static str),
        #[error(
            "incompatible proving stacks `{first_stack}` and `{second_stack}` share API-facing VK hash {verification_key_hash}"
        )]
        IncompatibleStacksShareVerificationKey {
            verification_key_hash: &'static str,
            first_stack: &'static str,
            second_stack: &'static str,
        },
    }

    fn validate_registry(registry: &ProvingRegistry) -> Result<(), ProvingRegistryInvariantError> {
        for (index, entry) in registry.entries.iter().enumerate() {
            validate_configuration(entry.configuration)?;

            for previous in &registry.entries[..index] {
                if previous.protocol_version == entry.protocol_version {
                    return Err(ProvingRegistryInvariantError::DuplicateProtocolVersion(
                        entry.protocol_version.clone(),
                    ));
                }

                let previous_configuration = previous.configuration;
                let configuration = entry.configuration;
                if previous_configuration.name == configuration.name
                    && !std::ptr::eq(previous_configuration, configuration)
                {
                    return Err(ProvingRegistryInvariantError::ConflictingStackName(
                        configuration.name,
                    ));
                }
                if previous_configuration.verification_key_hash
                    == configuration.verification_key_hash
                    && !std::ptr::eq(previous_configuration, configuration)
                {
                    return Err(
                        ProvingRegistryInvariantError::IncompatibleStacksShareVerificationKey {
                            verification_key_hash: configuration.verification_key_hash,
                            first_stack: previous_configuration.name,
                            second_stack: configuration.name,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_configuration(
        configuration: &ProvingStackConfiguration,
    ) -> Result<(), ProvingRegistryInvariantError> {
        let invalid = |reason| {
            ProvingRegistryInvariantError::InvalidConfiguration(configuration.name, reason)
        };

        if configuration.name.is_empty() {
            return Err(invalid("stack name is empty"));
        }
        let Some(hash) = configuration.verification_key_hash.strip_prefix("0x") else {
            return Err(invalid("verification key hash has no 0x prefix"));
        };
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("verification key hash is not 32-byte hex"));
        }
        if let FriProofConfiguration::UnrolledProof {
            application_end_params,
        } = configuration.fri
            && application_end_params.iter().all(|word| *word == 0)
        {
            return Err(invalid("application end parameters are all zero"));
        }
        Ok(())
    }

    #[test]
    fn production_registry_is_valid_and_round_trips_entries() {
        let registry = proving_registry();
        validate_registry(registry).unwrap();
        let mut protocol_versions = std::collections::HashSet::new();

        for entry in registry.iter() {
            assert!(protocol_versions.insert(entry.protocol_version.clone()));
            crate::ExecutionVersion::try_from(&entry.protocol_version)
                .expect("registered proving protocol must have a known execution version");
            let resolved = registry.get(&entry.protocol_version).unwrap();
            assert!(std::ptr::eq(resolved, entry.configuration));
            for other in registry.iter() {
                if entry.configuration.verification_key_hash
                    == other.configuration.verification_key_hash
                {
                    assert!(std::ptr::eq(entry.configuration, other.configuration));
                }
            }
        }
    }

    #[test]
    fn missing_exact_version_fails_closed() {
        static ENTRIES: [ProvingRegistryEntry; 1] = [ProvingRegistryEntry {
            protocol_version: ProtocolSemanticVersion::new(1, 0, 0),
            configuration: &ZK_OS_0_3_PROVING_STACK,
        }];
        let registry = ProvingRegistry { entries: &ENTRIES };
        let missing = ProtocolSemanticVersion::new(1, 0, 1);
        let error = registry.require(&missing, "registry test").unwrap_err();
        assert_eq!(error.protocol_version, missing);
    }

    #[test]
    fn distinct_stacks_cannot_share_an_api_verification_key() {
        static OTHER_STACK: ProvingStackConfiguration = ProvingStackConfiguration {
            name: "synthetic-distinct-stack",
            verification_key_hash: ZK_OS_0_3_VK_HASH,
            prover_input: ProverInputStrategy::ZkOs0_3BlockInputs,
            fri: FriProofConfiguration::ProgramProof,
            l1_verifier_selector: 0,
        };
        static ENTRIES: [ProvingRegistryEntry; 2] = [
            ProvingRegistryEntry {
                protocol_version: ProtocolSemanticVersion::new(1, 0, 0),
                configuration: &ZK_OS_0_3_PROVING_STACK,
            },
            ProvingRegistryEntry {
                protocol_version: ProtocolSemanticVersion::new(1, 0, 1),
                configuration: &OTHER_STACK,
            },
        ];
        let registry = ProvingRegistry { entries: &ENTRIES };

        assert!(matches!(
            validate_registry(&registry),
            Err(ProvingRegistryInvariantError::IncompatibleStacksShareVerificationKey { .. })
        ));
    }
}
