//! Registry-derivation fixtures for the simulator: manufactured registry chain
//! state, an in-memory derivation ledger, and a [`DerivationSource`] over slot
//! maps — so the deterministic cluster runs the *production* derivation driver
//! and the *production* registry parser, with only the chain-state transport
//! simulated.
//!
//! A scenario describes the registry's history as a timeline of full states:
//! "as of height H, the registry contains these slots". Governance writes land
//! at a height and persist until a later state supersedes them — exactly how a
//! derivation at a lookahead height observes an on-chain contract.

use alloy::primitives::{Address, B256};
use commonware_cryptography::Signer as _;
use commonware_cryptography::bls12381::primitives::group;
use commonware_cryptography::ed25519;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use zksync_os_consensus_core::registry::{
    DerivationAttempt, DerivationLedger, DerivationSource, RecordedDerivation, RegistryReading,
};
use zksync_os_consensus_core::schedule::Committee;
use zksync_os_consensus_registry::v1::{RawIdentity, RegistryStateBuilder};
use zksync_os_consensus_registry::{read_registry, sign_proof_of_possession};
use zksync_os_interface::traits::ReadStorage;

/// Where the simulated chain "deploys" the registry. Uniform across the
/// cluster, like the config value it models.
pub const REGISTRY_ADDRESS: Address = Address::repeat_byte(0x42);
/// The simulated chain id proofs of possession bind to.
pub const REGISTRY_CHAIN_ID: u64 = 6565;

/// One validator's registry identity from its cluster keys: real keys, a real
/// proof of possession, deterministic endpoints.
pub fn identity_for(index: usize, keys: &(ed25519::PrivateKey, group::Private)) -> RawIdentity {
    use commonware_codec::Encode as _;
    use commonware_cryptography::bls12381::primitives::ops;
    use commonware_cryptography::bls12381::primitives::variant::MinPk;
    let (network, bls) = keys;
    let owner = Address::repeat_byte(index as u8 + 1);
    let pop = sign_proof_of_possession(bls, owner, REGISTRY_CHAIN_ID, REGISTRY_ADDRESS);
    let mut bls_key = [0u8; 48];
    bls_key.copy_from_slice(&ops::compute_public::<MinPk>(bls).encode());
    let mut network_key = [0u8; 32];
    network_key.copy_from_slice(&network.public_key().encode());
    let mut pop_bytes = [0u8; 96];
    pop_bytes.copy_from_slice(&pop.encode());
    RawIdentity {
        owner,
        bls_key,
        network_key,
        pop: pop_bytes,
        ingress: format!("10.0.0.{}:3054", index + 1)
            .parse()
            .expect("socket"),
        egress: format!("10.0.0.{}", index + 1).parse().expect("ip"),
    }
}

/// A valid registry state: every cluster validator registered (in index order),
/// plus the given schedule entries (`activation_epoch` → member indices).
pub fn registry_state(
    keys: &[(ed25519::PrivateKey, group::Private)],
    entries: &[(u64, Vec<usize>)],
) -> Vec<(B256, B256)> {
    registry_builder(keys, entries).build()
}

/// The builder behind [`registry_state`], for scenarios that sabotage the state
/// before building (unknown layout versions, extra identities, …).
pub fn registry_builder(
    keys: &[(ed25519::PrivateKey, group::Private)],
    entries: &[(u64, Vec<usize>)],
) -> RegistryStateBuilder {
    let mut builder = RegistryStateBuilder::new(REGISTRY_ADDRESS);
    for (index, key) in keys.iter().enumerate() {
        builder = builder.identity(identity_for(index, key));
    }
    for (epoch, members) in entries {
        builder = builder.schedule_entry(
            *epoch,
            members.iter().map(|&member| member as u64).collect(),
        );
    }
    builder
}

/// Like [`registry_state`], but validator `victim`'s proof of possession is
/// valid cryptography signed for the wrong owner — the rogue-key/front-run
/// shape node-side verification exists to reject.
pub fn registry_state_with_bad_pop(
    keys: &[(ed25519::PrivateKey, group::Private)],
    entries: &[(u64, Vec<usize>)],
    victim: usize,
) -> Vec<(B256, B256)> {
    use commonware_codec::Encode as _;
    let mut builder = RegistryStateBuilder::new(REGISTRY_ADDRESS);
    for (index, key) in keys.iter().enumerate() {
        let mut identity = identity_for(index, key);
        if index == victim {
            let wrong_owner = Address::repeat_byte(0xEE);
            let pop =
                sign_proof_of_possession(&key.1, wrong_owner, REGISTRY_CHAIN_ID, REGISTRY_ADDRESS);
            identity.pop.copy_from_slice(&pop.encode());
        }
        builder = builder.identity(identity);
    }
    for (epoch, members) in entries {
        builder = builder.schedule_entry(
            *epoch,
            members.iter().map(|&member| member as u64).collect(),
        );
    }
    builder.build()
}

/// The registry's chain-state timeline: full flat-keyed states, keyed by the
/// height they became effective at. A derivation at height H reads the newest
/// state at or below H; before the first entry the registry is undeployed
/// (all-zero slots).
pub type RegistryTimeline = BTreeMap<u64, Vec<(B256, B256)>>;

/// In-memory [`DerivationLedger`]: survives validator restarts like a node's
/// disk (the cluster keeps it on the validator, outside the running stack).
#[derive(Clone, Default)]
pub struct MemoryLedger {
    records: Arc<Mutex<BTreeMap<u64, RecordedDerivation>>>,
}

impl DerivationLedger for MemoryLedger {
    fn load(&self) -> anyhow::Result<Vec<RecordedDerivation>> {
        Ok(self.records.lock().unwrap().values().cloned().collect())
    }

    fn record(&self, record: &RecordedDerivation) -> anyhow::Result<bool> {
        let mut records = self.records.lock().unwrap();
        if records.contains_key(&record.epoch) {
            return Ok(false);
        }
        records.insert(record.epoch, record.clone());
        Ok(true)
    }
}

impl MemoryLedger {
    /// Recorded derivations, ascending — the scenario assertion surface.
    pub fn records(&self) -> Vec<RecordedDerivation> {
        self.records.lock().unwrap().values().cloned().collect()
    }
}

/// [`ReadStorage`] over one manufactured state.
struct MapState(std::collections::HashMap<B256, B256>);

impl ReadStorage for MapState {
    fn read(&mut self, key: B256) -> Option<B256> {
        self.0.get(&key).copied()
    }
}

/// [`DerivationSource`] over a [`RegistryTimeline`], running the production
/// registry parser. Every derive call is journaled (the floor-restart scenario
/// asserts recorded epochs are replayed, never re-derived).
#[derive(Clone)]
pub struct SlotMapSource {
    pub timeline: Arc<RegistryTimeline>,
    /// Every `(epoch, lookahead_height)` this source was asked to derive.
    pub derive_calls: Arc<Mutex<Vec<(u64, u64)>>>,
}

impl SlotMapSource {
    pub fn new(timeline: Arc<RegistryTimeline>) -> Self {
        Self {
            timeline,
            derive_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl DerivationSource for SlotMapSource {
    fn derive(&mut self, epoch: u64, lookahead_height: u64) -> DerivationAttempt {
        self.derive_calls
            .lock()
            .unwrap()
            .push((epoch, lookahead_height));
        let state: std::collections::HashMap<B256, B256> = self
            .timeline
            .range(..=lookahead_height)
            .next_back()
            .map(|(_, slots)| slots.iter().copied().collect())
            .unwrap_or_default();
        // The outcome mapping below mirrors the node's `StateDerivationSource`
        // (consensus/execution/src/registry_source.rs) — the sim runs the same
        // parser and must judge its result the same way.
        DerivationAttempt::Reading(
            match read_registry(MapState(state), REGISTRY_ADDRESS, REGISTRY_CHAIN_ID) {
                Ok(view) => match view.committee_for(epoch) {
                    Some(members) => {
                        use commonware_utils::TryFromIterator as _;
                        match Committee::try_from_iter(
                            members
                                .iter()
                                .map(|identity| (identity.network_key.clone(), identity.bls_key)),
                        ) {
                            Ok(committee) => RegistryReading::Committee(committee),
                            Err(err) => RegistryReading::Refused(format!(
                                "registry entry does not form a committee: {err:?}"
                            )),
                        }
                    }
                    None => RegistryReading::NoEntry,
                },
                Err(refusal) if refusal.is_uninitialized() => RegistryReading::NoEntry,
                Err(refusal) => RegistryReading::Refused(refusal.to_string()),
            },
        )
    }
}
