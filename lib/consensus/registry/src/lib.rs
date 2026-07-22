//! The on-chain validator registry, node side.
//!
//! `contracts/src/ValidatorRegistry.sol` keeps committee membership in chain
//! state; this crate reads it back. Consensus never calls the contract — a
//! node reads the contract's storage slots directly out of its own finalized,
//! applied state, so every honest node decodes identical bytes and derives an
//! identical committee.
//!
//! The crate is versioned the way the wire formats are: [`read_registry`]
//! reads the layout version (slot 0, the one slot every layout guarantees
//! forever) and dispatches to that version's own reader module ([`v1`]).
//! Output types here are version-independent — a committee is a committee no
//! matter which layout stored it — while everything layout-specific (slot
//! maps, packing, builders, bytecode pins) stays inside its version module.
//! A new layout is a new module, never a patch on an old one.
//!
//! The paranoia budget lives in the readers: unknown layout versions,
//! malformed keys, duplicate or reused keys, colliding endpoints, invalid
//! proofs of possession, or a broken schedule all yield a [`RegistryRefusal`]
//! instead of a committee. The caller's contract is *refuse to rotate*: keep
//! the last known committee, alarm, and wait for governance or an upgrade to
//! fix the registry — every honest node refuses identically, so liveness is
//! preserved and nothing diverges.

mod pop;
pub mod v1;

pub use pop::{proof_of_possession_message, sign_proof_of_possession, verify_proof_of_possession};

use alloy::primitives::ruint::aliases::B160;
use alloy::primitives::{Address, B256, U256};
use commonware_cryptography::bls12381::primitives::variant::{MinPk, Variant};
use commonware_cryptography::ed25519;
use std::net::{IpAddr, SocketAddr};
use zksync_os_interface::traits::ReadStorage;

/// Layout versions this build can parse. A registry reporting anything else
/// is newer than the binary; the reader refuses and the operator rolls
/// binaries before governance activates the new layout.
pub const SUPPORTED_LAYOUT_VERSIONS: &[u64] = &[1];

/// Every layout, present and future, stores its version here. This is the one
/// cross-version invariant — the dispatch's version byte.
pub const SLOT_LAYOUT_VERSION: u64 = 0;

/// Hard ceilings; a registry exceeding them is a configuration accident.
pub const MAX_IDENTITIES: u64 = 4096;
pub const MAX_SCHEDULE_ENTRIES: u64 = 4096;
/// Mirrors the contract's `MAX_ENTRY_SIZE`.
pub const MAX_ENTRY_SIZE: u64 = 1024;

/// One registered validator identity, decoded and fully validated.
#[derive(Clone, Debug)]
pub struct RegistryIdentity {
    pub owner: Address,
    pub bls_key: <MinPk as Variant>::Public,
    pub network_key: ed25519::PublicKey,
    pub ingress: SocketAddr,
    pub egress: IpAddr,
}

/// One schedule entry: from `activation_epoch` on, the committee is `members`
/// (indices into the identity table).
#[derive(Clone, Debug)]
pub struct RegistryScheduleEntry {
    pub activation_epoch: u64,
    pub members: Vec<u64>,
}

/// A fully read and validated registry.
#[derive(Clone, Debug)]
pub struct RegistryView {
    pub identities: Vec<RegistryIdentity>,
    pub schedule: Vec<RegistryScheduleEntry>,
}

impl RegistryView {
    /// The committee governing `epoch`, if any schedule entry has activated
    /// by then: the members of the newest entry with
    /// `activation_epoch <= epoch`. `None` means the registry holds no
    /// applicable entry (a deployed-but-not-yet-populated registry).
    pub fn committee_for(&self, epoch: u64) -> Option<Vec<&RegistryIdentity>> {
        let entry = self
            .schedule
            .iter()
            .rev()
            .find(|entry| entry.activation_epoch <= epoch)?;
        Some(
            entry
                .members
                .iter()
                .map(|&index| &self.identities[index as usize])
                .collect(),
        )
    }
}

/// Why a registry read yielded no committee. Every variant means the same
/// thing operationally: do not rotate, alarm, keep following the last known
/// committee.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryRefusal {
    #[error(
        "registry reports layout version {found}, this build parses {SUPPORTED_LAYOUT_VERSIONS:?} \
         — roll binaries that understand the new layout"
    )]
    UnknownLayoutVersion { found: U256 },
    #[error("registry declares {found} identities (limit {MAX_IDENTITIES})")]
    TooManyIdentities { found: U256 },
    #[error("registry declares {found} schedule entries (limit {MAX_SCHEDULE_ENTRIES})")]
    TooManyScheduleEntries { found: U256 },
    #[error("identity {index} has an invalid {field}")]
    MalformedIdentity { index: u64, field: &'static str },
    #[error("identity {index} reuses a key already registered by identity {previous}")]
    KeyReused { index: u64, previous: u64 },
    #[error("identity {index} carries an invalid proof of possession")]
    InvalidProofOfPossession { index: u64 },
    #[error("schedule entry {index} has a malformed {field}")]
    MalformedEntry { index: u64, field: &'static str },
    #[error("schedule entry {index} is empty")]
    EmptyEntry { index: u64 },
    #[error("schedule entry {index} lists {found} members (limit {MAX_ENTRY_SIZE})")]
    EntryTooLarge { index: u64, found: U256 },
    #[error("schedule entry {index} references unknown identity {member}")]
    UnknownMember { index: u64, member: U256 },
    #[error("schedule entry {index} lists identity {member} twice")]
    DuplicateMember { index: u64, member: u64 },
    #[error("schedule entry {index}: identities {a} and {b} share an ingress address")]
    IngressCollision { index: u64, a: u64, b: u64 },
    #[error(
        "schedule entry {index} activates at epoch {found}, not after the previous entry ({previous})"
    )]
    NonMonotonicSchedule {
        index: u64,
        found: u64,
        previous: u64,
    },
}

impl RegistryRefusal {
    /// An all-zero layout-version slot is not an unknown layout — it is chain
    /// state with no registry in it (not yet deployed, or a not-yet-activated
    /// address). Consumers treat it like a registry with no applicable schedule
    /// entry: the pre-deployment steady state of a shadow rollout must read as
    /// "nothing scheduled", not alarm as an unparseable registry. (Every real
    /// layout writes its version in its constructor, so a deployed registry can
    /// never read as zero.)
    pub fn is_uninitialized(&self) -> bool {
        matches!(
            self,
            RegistryRefusal::UnknownLayoutVersion { found } if found.is_zero()
        )
    }
}

/// Reads and fully validates the registry at `address` from `state` (a view
/// over finalized, applied chain state at the caller's chosen height),
/// dispatching on the layout version the registry reports. `chain_id` binds
/// proof-of-possession verification to this chain.
pub fn read_registry(
    mut state: impl ReadStorage,
    address: Address,
    chain_id: u64,
) -> Result<RegistryView, RegistryRefusal> {
    let version_word = state
        .read(flat_key(address, U256::from(SLOT_LAYOUT_VERSION)))
        .unwrap_or_default();
    let version = U256::from_be_bytes(version_word.0);
    match version {
        version if version == U256::from(1u64) => v1::reader::read(state, address, chain_id),
        other => Err(RegistryRefusal::UnknownLayoutVersion { found: other }),
    }
}

/// The flat storage key under which the chain stores `slot` of the contract
/// at `address` — what `ReadStorage::read` takes. Chain storage semantics,
/// shared by every layout version.
pub fn flat_key(address: Address, slot: U256) -> B256 {
    let key = zk_ee::common_structs::derive_flat_storage_key(
        &B160::from_be_bytes(address.into_array()),
        &B256::from(slot.to_be_bytes::<32>()).0.into(),
    );
    key.as_u8_array().into()
}

#[cfg(test)]
mod tests {
    use super::v1::{RawIdentity, RegistryStateBuilder};
    use super::*;
    use commonware_codec::{DecodeExt as _, Encode as _};
    use commonware_cryptography::Signer as _;
    use commonware_cryptography::bls12381::primitives::{group, ops};
    use std::collections::HashMap;

    const CHAIN_ID: u64 = 6565;
    const REGISTRY: Address = Address::repeat_byte(0x42);

    struct MapState(HashMap<B256, B256>);

    impl ReadStorage for MapState {
        fn read(&mut self, key: B256) -> Option<B256> {
            self.0.get(&key).copied()
        }
    }

    /// A deterministic validator identity: keys derived from `seed`, proof of
    /// possession signed over the standard message.
    fn test_identity(seed: u8) -> (RawIdentity, group::Private) {
        let mut scalar = [0u8; 32];
        scalar[31] = seed;
        let bls_private = group::Private::decode(scalar.as_slice()).expect("small scalar is valid");
        let bls_public = ops::compute_public::<MinPk>(&bls_private);
        let owner = Address::repeat_byte(seed);
        let pop = sign_proof_of_possession(&bls_private, owner, CHAIN_ID, REGISTRY);

        let mut network_seed = [seed; 32];
        network_seed[0] = 0x10;
        let network_private =
            ed25519::PrivateKey::decode(network_seed.as_slice()).expect("any seed is a valid key");

        let mut bls_bytes = [0u8; 48];
        bls_bytes.copy_from_slice(&bls_public.encode());
        let mut network_bytes = [0u8; 32];
        network_bytes.copy_from_slice(&network_private.public_key().encode());
        let mut pop_bytes = [0u8; 96];
        pop_bytes.copy_from_slice(&pop.encode());

        (
            RawIdentity {
                owner,
                bls_key: bls_bytes,
                network_key: network_bytes,
                pop: pop_bytes,
                ingress: format!("10.0.0.{seed}:3054").parse().expect("socket"),
                egress: format!("10.0.0.{seed}").parse().expect("ip"),
            },
            bls_private,
        )
    }

    fn state_of(builder: RegistryStateBuilder) -> MapState {
        MapState(builder.build().into_iter().collect())
    }

    fn two_identity_builder() -> RegistryStateBuilder {
        RegistryStateBuilder::new(REGISTRY)
            .identity(test_identity(1).0)
            .identity(test_identity(2).0)
    }

    #[test]
    fn a_populated_registry_reads_back_and_derives_committees() {
        let builder = two_identity_builder()
            .identity(test_identity(3).0)
            .schedule_entry(0, vec![0, 1])
            .schedule_entry(7, vec![0, 1, 2]);
        let view = read_registry(state_of(builder), REGISTRY, CHAIN_ID).expect("valid registry");

        assert_eq!(view.identities.len(), 3);
        assert_eq!(view.identities[0].owner, Address::repeat_byte(1));
        assert_eq!(
            view.identities[1].ingress,
            "10.0.0.2:3054".parse().expect("socket"),
        );

        let early = view.committee_for(3).expect("entry 0 applies");
        assert_eq!(early.len(), 2);
        let late = view.committee_for(7).expect("entry 1 applies");
        assert_eq!(late.len(), 3);
        assert_eq!(view.committee_for(u64::MAX).expect("latest").len(), 3);
    }

    #[test]
    fn an_empty_registry_reads_back_with_no_committee() {
        let view = read_registry(
            state_of(RegistryStateBuilder::new(REGISTRY)),
            REGISTRY,
            CHAIN_ID,
        )
        .expect("an empty registry is valid");
        assert!(view.committee_for(0).is_none());
    }

    #[test]
    fn an_unknown_layout_version_is_refused() {
        let builder = two_identity_builder()
            .schedule_entry(0, vec![0, 1])
            .with_layout_version(2);
        let refusal = read_registry(state_of(builder), REGISTRY, CHAIN_ID).unwrap_err();
        assert!(matches!(
            refusal,
            RegistryRefusal::UnknownLayoutVersion { .. }
        ));
    }

    /// Only the all-zero layout version reads as "not deployed yet" — a real
    /// unknown version and every other refusal must alarm, never blend into
    /// the shadow-rollout steady state.
    #[test]
    fn only_the_zero_layout_version_reads_as_uninitialized() {
        assert!(RegistryRefusal::UnknownLayoutVersion { found: U256::ZERO }.is_uninitialized());
        assert!(
            !RegistryRefusal::UnknownLayoutVersion {
                found: U256::from(2)
            }
            .is_uninitialized()
        );
        assert!(!RegistryRefusal::TooManyIdentities { found: U256::ZERO }.is_uninitialized());
    }

    #[test]
    fn a_proof_of_possession_for_the_wrong_context_is_refused() {
        // The proof is valid cryptography, signed for a different owner —
        // exactly the front-run/replay shape the binding exists to stop.
        let (mut identity, bls_private) = test_identity(1);
        let wrong_owner = Address::repeat_byte(0xEE);
        let pop = sign_proof_of_possession(&bls_private, wrong_owner, CHAIN_ID, REGISTRY);
        identity.pop.copy_from_slice(&pop.encode());
        let builder = RegistryStateBuilder::new(REGISTRY).identity(identity);
        let refusal = read_registry(state_of(builder), REGISTRY, CHAIN_ID).unwrap_err();
        assert_eq!(
            refusal,
            RegistryRefusal::InvalidProofOfPossession { index: 0 },
        );
    }

    #[test]
    fn a_reused_key_is_refused() {
        let (mut copycat, _) = test_identity(3);
        copycat.bls_key = test_identity(1).0.bls_key;
        let builder = two_identity_builder().identity(copycat);
        let refusal = read_registry(state_of(builder), REGISTRY, CHAIN_ID).unwrap_err();
        assert_eq!(
            refusal,
            RegistryRefusal::KeyReused {
                index: 2,
                previous: 0,
            },
        );
    }

    #[test]
    fn schedule_problems_are_refused() {
        let colliding = {
            let (mut identity, _) = test_identity(3);
            identity.ingress = test_identity(1).0.ingress;
            identity
        };
        let cases: Vec<(RegistryStateBuilder, RegistryRefusal)> = vec![
            (
                two_identity_builder()
                    .schedule_entry(5, vec![0, 1])
                    .schedule_entry(5, vec![0, 1]),
                RegistryRefusal::NonMonotonicSchedule {
                    index: 1,
                    found: 5,
                    previous: 5,
                },
            ),
            (
                two_identity_builder().schedule_entry(0, vec![0, 7]),
                RegistryRefusal::UnknownMember {
                    index: 0,
                    member: U256::from(7u64),
                },
            ),
            (
                two_identity_builder().schedule_entry(0, vec![1, 1]),
                RegistryRefusal::DuplicateMember {
                    index: 0,
                    member: 1,
                },
            ),
            (
                two_identity_builder()
                    .identity(colliding)
                    .schedule_entry(0, vec![0, 2]),
                RegistryRefusal::IngressCollision {
                    index: 0,
                    a: 0,
                    b: 2,
                },
            ),
            (
                two_identity_builder().schedule_entry(0, vec![]),
                RegistryRefusal::EmptyEntry { index: 0 },
            ),
        ];
        for (builder, expected) in cases {
            let refusal = read_registry(state_of(builder), REGISTRY, CHAIN_ID).unwrap_err();
            assert_eq!(refusal, expected);
        }
    }

    #[test]
    fn socket_packing_round_trips() {
        use super::v1::{pack_egress, pack_ingress, unpack_egress, unpack_ingress};
        let v4: SocketAddr = "10.1.2.3:3054".parse().expect("socket");
        assert_eq!(unpack_ingress(pack_ingress(v4)), Some(v4));
        let v6: SocketAddr = "[2001:db8::17]:9000".parse().expect("socket");
        assert_eq!(unpack_ingress(pack_ingress(v6)), Some(v6));
        let ip: IpAddr = "192.168.0.9".parse().expect("ip");
        assert_eq!(unpack_egress(pack_egress(ip)), Some(ip));

        // A zero port is not a listenable ingress; junk version tags and dirty
        // reserved bytes are not addresses at all.
        assert_eq!(
            unpack_ingress(pack_egress(ip)),
            None,
            "an egress word is not a valid ingress",
        );
        assert_eq!(unpack_ingress(B256::ZERO), None);
        let mut dirty = pack_ingress(v4).0;
        dirty[31] = 1;
        assert_eq!(unpack_ingress(B256::from(dirty)), None);
    }
}
