//! The node-side implementations of the registry-derivation seams: reading the
//! validator registry out of the node's own state backend, and persisting the
//! derivation trail in the finality store.
//!
//! The derivation *logic* lives in `zksync_os_consensus_core::registry` (where
//! the simulator can drive it); this module supplies the two production pieces
//! it is generic over — [`StateDerivationSource`] (chain state → registry
//! reading, via the registry crate's slot parser) and the
//! [`DerivationLedger`] implementation over [`FinalityStore`].

use crate::finality_store::FinalityStore;
use alloy::primitives::Address;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use zksync_os_consensus_core::registry::{
    DerivationAttempt, DerivationLedger, DerivationSource, RecordedDerivation, RecordedOutcome,
    RegistryReading,
};
use zksync_os_consensus_core::schedule::Committee;
use zksync_os_consensus_registry::{RegistryIdentity, read_registry};
use zksync_os_interface::traits::ReadStorage;
use zksync_os_storage_api::ReadStateHistory;

/// Reads the registry from the node's state backend at a fixed height.
///
/// Availability is separated from content: a height whose state view cannot be
/// opened (not yet applied, pruned, backend hiccup) yields
/// [`DerivationAttempt::Unavailable`] — retried, never recorded — while anything
/// the registry parser says about *readable* state is a chain fact. A read
/// failing mid-parse poisons the whole attempt back to `Unavailable`: a
/// half-read registry must never masquerade as a refusal, because a refusal is
/// recorded and must be byte-identical on every node.
pub struct StateDerivationSource<S> {
    base: S,
    registry_address: Address,
    chain_id: u64,
}

impl<S> StateDerivationSource<S> {
    pub fn new(base: S, registry_address: Address, chain_id: u64) -> Self {
        Self {
            base,
            registry_address,
            chain_id,
        }
    }
}

/// [`ReadStorage`] over a state view at a fixed height that reports failures
/// instead of panicking (the environment's `BaseViewAt` promises "committed
/// state is available"; a derivation makes no such promise). The flag outlives
/// the reader because `read_registry` consumes it by value.
struct FallibleViewAt<S> {
    base: S,
    height: u64,
    poisoned: Arc<AtomicBool>,
}

impl<S: ReadStateHistory + Clone + Send + 'static> ReadStorage for FallibleViewAt<S> {
    fn read(&mut self, key: alloy::primitives::B256) -> Option<alloy::primitives::B256> {
        match self.base.state_view_at(self.height) {
            Ok(mut view) => view.read(key),
            Err(_) => {
                self.poisoned.store(true, Ordering::Relaxed);
                None
            }
        }
    }
}

impl<S: ReadStateHistory + Clone + Send + Sync + 'static> DerivationSource
    for StateDerivationSource<S>
{
    fn derive(&mut self, epoch: u64, lookahead_height: u64) -> DerivationAttempt {
        // Probe availability first so the common failure (state not there) never
        // even starts a parse.
        if self.base.state_view_at(lookahead_height).is_err() {
            return DerivationAttempt::Unavailable;
        }
        let poisoned = Arc::new(AtomicBool::new(false));
        let reader = FallibleViewAt {
            base: self.base.clone(),
            height: lookahead_height,
            poisoned: poisoned.clone(),
        };
        let outcome = read_registry(reader, self.registry_address, self.chain_id);
        if poisoned.load(Ordering::Relaxed) {
            return DerivationAttempt::Unavailable;
        }
        DerivationAttempt::Reading(match outcome {
            Ok(view) => match view.committee_for(epoch) {
                Some(members) => match registry_committee(&members) {
                    Ok(committee) => RegistryReading::Committee(committee),
                    Err(reason) => RegistryReading::Refused(reason),
                },
                None => RegistryReading::NoEntry,
            },
            // Undeployed (all-zero) reads as "nothing scheduled", not as a
            // refusal — the steady state of every shadow rollout until
            // governance deploys.
            Err(refusal) if refusal.is_uninitialized() => RegistryReading::NoEntry,
            Err(refusal) => RegistryReading::Refused(refusal.to_string()),
        })
    }
}

/// The registry's member list as a consensus committee, in entry order (the
/// registry's order is the agreement — certificate bitmaps index into it).
/// Duplicate keys cannot pass the registry's own validation; the error arm is
/// defense in depth and maps to a refusal like any other invalid set.
fn registry_committee(members: &[&RegistryIdentity]) -> Result<Committee, String> {
    use commonware_utils::TryFromIterator as _;
    Committee::try_from_iter(
        members
            .iter()
            .map(|identity| (identity.network_key.clone(), identity.bls_key)),
    )
    .map_err(|err| format!("registry entry does not form a committee: {err:?}"))
}

/// The derivation trail persisted next to the finality certificates it
/// parallels (the finality store outlives every consensus-library format).
#[derive(Clone)]
pub struct RegistryLedger(pub Arc<FinalityStore>);

impl DerivationLedger for RegistryLedger {
    fn load(&self) -> anyhow::Result<Vec<RecordedDerivation>> {
        self.0
            .registry_derivations()?
            .into_iter()
            .map(from_wire)
            .collect()
    }

    fn record(&self, record: &RecordedDerivation) -> anyhow::Result<bool> {
        self.0.record_registry_derivation(&to_wire(record))
    }
}

fn to_wire(record: &RecordedDerivation) -> zksync_os_wire::RegistryDerivation {
    use commonware_codec::Encode as _;
    zksync_os_wire::RegistryDerivation {
        epoch: record.epoch,
        lookahead_height: record.lookahead_height,
        outcome: match record.outcome {
            RecordedOutcome::Derived => zksync_os_wire::DerivationOutcome::Derived,
            RecordedOutcome::CarriedNoEntry => zksync_os_wire::DerivationOutcome::CarriedNoEntry,
            RecordedOutcome::CarriedRefused => zksync_os_wire::DerivationOutcome::CarriedRefused,
        },
        committee: record
            .committee
            .iter_pairs()
            .map(
                |(network_key, bls_key)| zksync_os_wire::CommitteeMemberKeys {
                    network_key: network_key
                        .encode()
                        .as_ref()
                        .try_into()
                        .expect("ed25519 public keys encode to 32 bytes"),
                    bls_key: bls_key
                        .encode()
                        .as_ref()
                        .try_into()
                        .expect("BLS12-381 MinPk public keys encode to 48 bytes"),
                },
            )
            .collect(),
    }
}

fn from_wire(record: zksync_os_wire::RegistryDerivation) -> anyhow::Result<RecordedDerivation> {
    use commonware_codec::DecodeExt as _;
    use commonware_utils::TryFromIterator as _;
    let committee = Committee::try_from_iter(record.committee.iter().map(|member| {
        let network_key =
            commonware_cryptography::ed25519::PublicKey::decode(member.network_key.as_slice())
                .expect("32 bytes decode as an ed25519 public key");
        let bls_key = <commonware_cryptography::bls12381::primitives::variant::MinPk as commonware_cryptography::bls12381::primitives::variant::Variant>::Public::decode(
            member.bls_key.as_slice(),
        )
        .map_err(|err| anyhow::anyhow!("stored BLS key does not decode: {err}"))?;
        Ok::<_, anyhow::Error>((network_key, bls_key))
    }).collect::<Result<Vec<_>, _>>()?)
    .map_err(|err| anyhow::anyhow!("stored derivation does not form a committee: {err:?}"))?;
    Ok(RecordedDerivation {
        epoch: record.epoch,
        lookahead_height: record.lookahead_height,
        outcome: match record.outcome {
            zksync_os_wire::DerivationOutcome::Derived => RecordedOutcome::Derived,
            zksync_os_wire::DerivationOutcome::CarriedNoEntry => RecordedOutcome::CarriedNoEntry,
            zksync_os_wire::DerivationOutcome::CarriedRefused => RecordedOutcome::CarriedRefused,
        },
        committee,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::Signer as _;

    fn test_committee(seeds: &[u8]) -> Committee {
        use commonware_codec::DecodeExt as _;
        use commonware_cryptography::bls12381::primitives::{group, ops};
        use commonware_utils::TryFromIterator as _;
        Committee::try_from_iter(
            seeds.iter().map(|&seed| {
                let network =
                    commonware_cryptography::ed25519::PrivateKey::decode([seed; 32].as_slice())
                        .expect("seed");
                let mut scalar = [0u8; 32];
                scalar[31] = seed;
                let bls = group::Private::decode(scalar.as_slice()).expect("small scalar");
                (
                    network.public_key(),
                    ops::compute_public::<
                        commonware_cryptography::bls12381::primitives::variant::MinPk,
                    >(&bls),
                )
            }),
        )
        .expect("distinct keys")
    }

    fn test_identity(seed: u8) -> RegistryIdentity {
        use commonware_codec::DecodeExt as _;
        use commonware_cryptography::bls12381::primitives::{group, ops};
        let network = commonware_cryptography::ed25519::PrivateKey::decode([seed; 32].as_slice())
            .expect("seed");
        let mut scalar = [0u8; 32];
        scalar[31] = seed;
        let bls = group::Private::decode(scalar.as_slice()).expect("small scalar");
        RegistryIdentity {
            owner: Address::repeat_byte(seed),
            bls_key: ops::compute_public::<
                commonware_cryptography::bls12381::primitives::variant::MinPk,
            >(&bls),
            network_key: network.public_key(),
            ingress: "127.0.0.1:3000".parse().expect("socket addr"),
            egress: "127.0.0.1".parse().expect("ip addr"),
        }
    }

    #[test]
    fn registry_members_become_the_committee_in_entry_order() {
        let first = test_identity(1);
        let second = test_identity(2);
        let committee = registry_committee(&[&first, &second]).expect("distinct keys");
        // The registry's entry order *is* the agreement — certificate bitmaps
        // index into it — and every member must be present.
        assert_eq!(committee, test_committee(&[1, 2]));

        // Duplicate keys cannot form a committee: refusal, not silent dedup.
        assert!(registry_committee(&[&first, &first]).is_err());
    }

    /// An absent (all-zero) registry must read as "nothing scheduled", never as
    /// a recorded refusal: undeployed is the steady state of every shadow
    /// rollout, and a refusal is a permanent chain fact.
    #[test]
    fn an_undeployed_registry_reads_as_no_entry_not_a_refusal() {
        #[derive(Clone, Debug)]
        struct EmptyView;
        impl zksync_os_interface::traits::ReadStorage for EmptyView {
            fn read(&mut self, _key: alloy::primitives::B256) -> Option<alloy::primitives::B256> {
                None
            }
        }
        impl zksync_os_interface::traits::PreimageSource for EmptyView {
            fn get_preimage(&mut self, _hash: alloy::primitives::B256) -> Option<Vec<u8>> {
                None
            }
        }
        #[derive(Clone, Debug)]
        struct EmptyState;
        impl ReadStateHistory for EmptyState {
            fn state_view_at(
                &self,
                _block_number: u64,
            ) -> zksync_os_storage_api::StateResult<impl zksync_os_storage_api::ViewState>
            {
                Ok(EmptyView)
            }
            fn block_range_available(&self) -> std::ops::RangeInclusive<u64> {
                0..=u64::MAX
            }
        }

        let mut source = StateDerivationSource::new(EmptyState, Address::repeat_byte(0x42), 270);
        match source.derive(1, 10) {
            DerivationAttempt::Reading(RegistryReading::NoEntry) => {}
            other => panic!("undeployed registry must read as NoEntry, got {other:?}"),
        }
    }

    #[test]
    fn ledger_roundtrips_records_first_observed_wins_and_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = |epoch: u64, seeds: &[u8], outcome: RecordedOutcome| RecordedDerivation {
            epoch,
            lookahead_height: 100 * epoch,
            outcome,
            committee: test_committee(seeds),
        };
        {
            let store = RegistryLedger(Arc::new(FinalityStore::open(dir.path()).expect("open")));
            assert!(store.load().expect("load").is_empty());
            assert!(
                store
                    .record(&record(3, &[1, 2], RecordedOutcome::Derived))
                    .expect("write")
            );
            assert!(
                store
                    .record(&record(4, &[1, 2, 3], RecordedOutcome::CarriedRefused))
                    .expect("write")
            );
            // First-observed wins: a re-derivation with different content leaves
            // the original untouched.
            assert!(
                !store
                    .record(&record(3, &[7, 8], RecordedOutcome::CarriedNoEntry))
                    .expect("write")
            );
        }
        let store = RegistryLedger(Arc::new(FinalityStore::open(dir.path()).expect("reopen")));
        let loaded = store.load().expect("load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].epoch, 3, "records load ascending by epoch");
        assert_eq!(loaded[0].outcome, RecordedOutcome::Derived);
        assert_eq!(loaded[0].committee, test_committee(&[1, 2]));
        assert_eq!(loaded[1].epoch, 4);
        assert_eq!(loaded[1].outcome, RecordedOutcome::CarriedRefused);
        assert_eq!(loaded[1].committee.len(), 3);
    }
}
