use std::sync::OnceLock;

use super::ProtocolSemanticVersion;

/// Concrete implementation used to produce the prover input for a sealed batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProverInputStrategy {
    /// Generate ZKsync OS 0.2 inputs per block and combine them when sealing the batch.
    ZksyncOs02BlockInputs,
    /// Generate ZKsync OS 0.3 inputs per block and combine them when sealing the batch.
    ZksyncOs03BlockInputs,
    /// Preserve per-block tree data and re-execute the batch with the ZKsync OS 0.4 program.
    ZksyncOs04NativeBatch,
}

impl ProverInputStrategy {
    pub const fn requires_native_batch_run(self) -> bool {
        match self {
            Self::ZksyncOs02BlockInputs | Self::ZksyncOs03BlockInputs => false,
            Self::ZksyncOs04NativeBatch => true,
        }
    }
}

/// Decoding and verification implementation for opaque FRI proof bytes in Prover API v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriVerificationStrategy {
    /// Decode an Airbender `ProgramProof`; its application-specific VK binds the program.
    AirbenderProgramProof,
    /// Decode an Airbender `UnrolledProgramProof` and bind it to the registered application.
    AirbenderUnifiedLayer {
        application_end_params: &'static [u32; 8],
    },
}

/// Construction of the hash exposed in the proof's public-input registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriPublicInputLayout {
    StateTransitionAndBatchOutput,
    StateTransitionChainConfigAndBatchOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FriProofConfiguration {
    pub verification: FriVerificationStrategy,
    pub public_input_layout: FriPublicInputLayout,
}

/// Identity used to decide whether adjacent FRI proofs may be aggregated together.
///
/// It is deliberately opaque: consumers may compare identities, but protocol-to-stack
/// selection remains owned by this registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregationGroup(&'static str);

/// Complete release-specific configuration for one reusable proving stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvingStackConfiguration {
    /// A diagnostic name, not a serialized or independently numbered version.
    pub name: &'static str,
    pub verification_key_hash: &'static str,
    pub prover_input: ProverInputStrategy,
    pub fri: FriProofConfiguration,
    pub aggregation_group: AggregationGroup,
    /// Numeric selector encoded in the existing L1 proof calldata.
    pub l1_verifier_selector: u32,
}

impl ProvingStackConfiguration {
    /// Diagnostic names do not affect prover assignment or proof compatibility.
    fn operationally_equivalent(&self, other: &Self) -> bool {
        self.verification_key_hash == other.verification_key_hash
            && self.prover_input == other.prover_input
            && self.fri == other.fri
            && self.aggregation_group == other.aggregation_group
            && self.l1_verifier_selector == other.l1_verifier_selector
    }
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

    pub fn contains_verification_key_hash(&self, verification_key_hash: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.configuration.verification_key_hash == verification_key_hash)
    }

    fn validate(&self) -> Result<(), ProvingRegistryInvariantError> {
        for (index, entry) in self.entries.iter().enumerate() {
            validate_configuration(entry.configuration)?;

            for previous in &self.entries[..index] {
                if previous.protocol_version == entry.protocol_version {
                    return Err(ProvingRegistryInvariantError::DuplicateProtocolVersion(
                        entry.protocol_version.clone(),
                    ));
                }

                let previous_configuration = previous.configuration;
                let configuration = entry.configuration;
                if previous_configuration.name == configuration.name
                    && !previous_configuration.operationally_equivalent(configuration)
                {
                    return Err(ProvingRegistryInvariantError::ConflictingStackName(
                        configuration.name,
                    ));
                }
                if previous_configuration.verification_key_hash
                    == configuration.verification_key_hash
                    && !previous_configuration.operationally_equivalent(configuration)
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
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error(
    "protocol version {protocol_version} is not registered for proving; required by {operation}"
)]
pub struct UnsupportedProtocolForProving {
    pub protocol_version: ProtocolSemanticVersion,
    pub operation: &'static str,
}

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

fn validate_configuration(
    configuration: &ProvingStackConfiguration,
) -> Result<(), ProvingRegistryInvariantError> {
    let invalid =
        |reason| ProvingRegistryInvariantError::InvalidConfiguration(configuration.name, reason);

    if configuration.name.is_empty() {
        return Err(invalid("stack name is empty"));
    }
    let Some(hash) = configuration.verification_key_hash.strip_prefix("0x") else {
        return Err(invalid("verification key hash has no 0x prefix"));
    };
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("verification key hash is not 32-byte hex"));
    }
    if let FriVerificationStrategy::AirbenderUnifiedLayer {
        application_end_params,
    } = configuration.fri.verification
        && application_end_params.iter().all(|word| *word == 0)
    {
        return Err(invalid("application end parameters are all zero"));
    }
    if configuration.aggregation_group.0.is_empty() {
        return Err(invalid("aggregation group is empty"));
    }
    Ok(())
}

const ZKSYNC_OS_0_2_AGGREGATION_GROUP: AggregationGroup =
    AggregationGroup("zksync-os-0.2-airbender-program-proof");
const ZKSYNC_OS_0_3_AGGREGATION_GROUP: AggregationGroup =
    AggregationGroup("zksync-os-0.3-airbender-program-proof");
const ZKSYNC_OS_0_4_AGGREGATION_GROUP: AggregationGroup =
    AggregationGroup("zksync-os-0.4-airbender-unified-proof");

// VK generated from ZKsync OS 0.2.5, Airbender 0.5.2, and zkos-wrapper 0.5.4.
const ZKSYNC_OS_0_2_PROVING_STACK: ProvingStackConfiguration = ProvingStackConfiguration {
    name: "zksync-os-0.2-airbender-program-proof",
    verification_key_hash: "0x124ebcd537a1e1c152774dd18f67660e35625bba0b669bf3b4836d636b105337",
    prover_input: ProverInputStrategy::ZksyncOs02BlockInputs,
    fri: FriProofConfiguration {
        verification: FriVerificationStrategy::AirbenderProgramProof,
        public_input_layout: FriPublicInputLayout::StateTransitionAndBatchOutput,
    },
    aggregation_group: ZKSYNC_OS_0_2_AGGREGATION_GROUP,
    l1_verifier_selector: 6,
};

// VK generated from ZKsync OS 0.3, Airbender 0.5.2, and zkos-wrapper 0.5.5.
const ZKSYNC_OS_0_3_PROVING_STACK: ProvingStackConfiguration = ProvingStackConfiguration {
    name: "zksync-os-0.3-airbender-program-proof",
    verification_key_hash: "0x23156cf220288cd1e436dccfc09aa4883ea8288da61aa69e2c7251b0c0c44ccd",
    prover_input: ProverInputStrategy::ZksyncOs03BlockInputs,
    fri: FriProofConfiguration {
        verification: FriVerificationStrategy::AirbenderProgramProof,
        public_input_layout: FriPublicInputLayout::StateTransitionAndBatchOutput,
    },
    aggregation_group: ZKSYNC_OS_0_3_AGGREGATION_GROUP,
    l1_verifier_selector: 0,
};

/// `end_params` of the ZKsync OS 0.4 multiblock batch program, built from draft-0.4.0 @ 8ef47499
/// (md5 `8128c18a3b7145366b184e027d0e0f34`). The unified FRI verifier is
/// application-independent, so this value binds its proof to the registered batch program.
const ZKSYNC_OS_0_4_APP_END_PARAMS: [u32; 8] = [
    2307768600, 2457250828, 3716327079, 4199813212, 118680239, 3956473405, 1127792062, 2161297246,
];

// VK generated from Airbender v0.6.0-rc.2 and zkos-wrapper v0.6.0-rc.2 at 100-bit security.
// The wrapper checks the program commitment, so the VK is specific to the registered batch
// program. Local unified FRI verification additionally binds it through the application end params.
const ZKSYNC_OS_0_4_PROVING_STACK: ProvingStackConfiguration = ProvingStackConfiguration {
    name: "zksync-os-0.4-airbender-unified-proof",
    verification_key_hash: "0xa81dc850a0724bcd62b7e5fbe60c62be32b4b45e33dd0d950f9c313e4684605a",
    prover_input: ProverInputStrategy::ZksyncOs04NativeBatch,
    fri: FriProofConfiguration {
        verification: FriVerificationStrategy::AirbenderUnifiedLayer {
            application_end_params: &ZKSYNC_OS_0_4_APP_END_PARAMS,
        },
        public_input_layout: FriPublicInputLayout::StateTransitionChainConfigAndBatchOutput,
    },
    aggregation_group: ZKSYNC_OS_0_4_AGGREGATION_GROUP,
    l1_verifier_selector: 8,
};

static REGISTRY_ENTRIES: [ProvingRegistryEntry; 5] = [
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 30, 1),
        configuration: &ZKSYNC_OS_0_2_PROVING_STACK,
    },
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 30, 2),
        configuration: &ZKSYNC_OS_0_2_PROVING_STACK,
    },
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 31, 0),
        configuration: &ZKSYNC_OS_0_3_PROVING_STACK,
    },
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 31, 1),
        configuration: &ZKSYNC_OS_0_3_PROVING_STACK,
    },
    ProvingRegistryEntry {
        protocol_version: ProtocolSemanticVersion::new(0, 32, 0),
        configuration: &ZKSYNC_OS_0_4_PROVING_STACK,
    },
];

pub fn proving_registry() -> &'static ProvingRegistry {
    static REGISTRY: OnceLock<ProvingRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let registry = ProvingRegistry {
            entries: &REGISTRY_ENTRIES,
        };
        registry
            .validate()
            .expect("authoritative proving registry is invalid");
        registry
    })
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

    fn unregistered_version(registry: &ProvingRegistry) -> ProtocolSemanticVersion {
        let base = registry
            .iter()
            .max_by(|left, right| left.protocol_version.cmp(&right.protocol_version))
            .expect("production registry must not be empty")
            .protocol_version
            .clone();
        let mut patch = base.patch.checked_add(1).unwrap_or(0);
        let minor = base.minor + u64::from(patch == 0);
        loop {
            let candidate = ProtocolSemanticVersion::new(base.major, minor, patch);
            if registry.get(&candidate).is_none() {
                return candidate;
            }
            patch = patch
                .checked_add(1)
                .expect("registry cannot contain every patch");
        }
    }

    #[test]
    fn production_registry_is_valid_and_round_trips_entries() {
        let registry = proving_registry();
        registry.validate().unwrap();
        let mut protocol_versions = std::collections::HashSet::new();

        for entry in registry.iter() {
            assert!(protocol_versions.insert(entry.protocol_version.clone()));
            let resolved = registry.get(&entry.protocol_version).unwrap();
            assert!(std::ptr::eq(resolved, entry.configuration));
        }
    }

    #[test]
    fn missing_exact_version_fails_closed() {
        let registry = proving_registry();
        let missing = unregistered_version(registry);
        let error = registry.require(&missing, "registry test").unwrap_err();
        assert_eq!(error.protocol_version, missing);
    }

    #[test]
    fn incompatible_stacks_cannot_share_an_api_verification_key() {
        static OTHER_STACK: ProvingStackConfiguration = ProvingStackConfiguration {
            name: "synthetic-incompatible-stack",
            aggregation_group: ZKSYNC_OS_0_4_AGGREGATION_GROUP,
            ..ZKSYNC_OS_0_3_PROVING_STACK
        };
        static ENTRIES: [ProvingRegistryEntry; 2] = [
            ProvingRegistryEntry {
                protocol_version: ProtocolSemanticVersion::new(1, 0, 0),
                configuration: &ZKSYNC_OS_0_3_PROVING_STACK,
            },
            ProvingRegistryEntry {
                protocol_version: ProtocolSemanticVersion::new(1, 0, 1),
                configuration: &OTHER_STACK,
            },
        ];
        let registry = ProvingRegistry { entries: &ENTRIES };

        assert!(matches!(
            registry.validate(),
            Err(ProvingRegistryInvariantError::IncompatibleStacksShareVerificationKey { .. })
        ));
    }

    #[test]
    fn diagnostic_names_do_not_affect_stack_compatibility() {
        static ALIASED_STACK: ProvingStackConfiguration = ProvingStackConfiguration {
            name: "synthetic-diagnostic-alias",
            ..ZKSYNC_OS_0_3_PROVING_STACK
        };
        static ENTRIES: [ProvingRegistryEntry; 2] = [
            ProvingRegistryEntry {
                protocol_version: ProtocolSemanticVersion::new(1, 0, 0),
                configuration: &ZKSYNC_OS_0_3_PROVING_STACK,
            },
            ProvingRegistryEntry {
                protocol_version: ProtocolSemanticVersion::new(1, 0, 1),
                configuration: &ALIASED_STACK,
            },
        ];

        ProvingRegistry { entries: &ENTRIES }.validate().unwrap();
    }
}
