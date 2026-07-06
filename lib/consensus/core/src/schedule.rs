//! The committee schedule: which validator set holds which epochs.
//!
//! The validator set is fixed *within* an epoch and may change only at epoch
//! boundaries. The schedule is the committee-wide agreement on those changes: an
//! ordered list of entries, each saying "from epoch E onward, this is the
//! committee" — the entry with the highest activation at or below an epoch wins.
//! Reconfiguration is therefore an append: every operator deploys a config with a
//! new entry *before* its activation epoch arrives, and the chain crosses the
//! boundary into the new committee with no further coordination.
//!
//! The schedule is configuration, and like every other committee-wide chain
//! constant it must be identical across validators. There is no in-band mechanism
//! that could reconcile two validators running different schedules — they would
//! disagree on whose signatures make a quorum, which is precisely the thing
//! consensus cannot decide for itself. A mismatch is loud (certificates fail to
//! verify, engines refuse each other's votes), not silent. The durable audit trail
//! of which committee actually held each epoch is the transition-record store; the
//! schedule here is the *forward-looking* declaration.
//!
//! [`ScheduledSchemeProvider`] turns the schedule into per-epoch signing schemes:
//! the signer scheme for epochs where this validator is a member, a verifier-only
//! scheme everywhere else — so certificates from any epoch in the chain's history
//! remain verifiable (backfill and catch-up depend on that), while "may I vote in
//! epoch E" stays a plain membership question.

use crate::types::Scheme;
use commonware_consensus::types::Epoch;
use commonware_cryptography::bls12381::primitives::group;
use commonware_cryptography::bls12381::primitives::variant::{MinPk, Variant};
use commonware_cryptography::ed25519::PublicKey;
use commonware_utils::ordered::BiMap;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// One schedule entry: the committee that holds every epoch from
/// `activation_epoch` until a later entry supersedes it.
#[derive(Clone, Debug)]
pub struct ScheduleEntry {
    pub activation_epoch: u64,
    /// The ordered (network identity → consensus key) map. Order is part of the
    /// agreement: participant indices in certificate bitmaps follow it, so every
    /// validator must configure the same members in the same order.
    pub committee: BiMap<PublicKey, <MinPk as Variant>::Public>,
}

/// The validated schedule. Construction enforces the invariants; everything after
/// construction is a total, infallible lookup.
#[derive(Clone, Debug)]
pub struct CommitteeSchedule {
    /// Sorted by strictly increasing `activation_epoch`, first entry at epoch 0.
    entries: Vec<ScheduleEntry>,
}

impl CommitteeSchedule {
    /// Validates and builds a schedule.
    ///
    /// Invariants: at least one entry; the first entry activates at epoch 0 (the
    /// schedule is a *total* history — every epoch a certificate could name must
    /// resolve to a committee, or verification of backfilled history would have
    /// holes); strictly increasing activations; no empty committees.
    pub fn new(entries: Vec<ScheduleEntry>) -> anyhow::Result<Self> {
        anyhow::ensure!(!entries.is_empty(), "committee schedule has no entries");
        anyhow::ensure!(
            entries[0].activation_epoch == 0,
            "the first schedule entry must activate at epoch 0 so every epoch in \
             history resolves to a committee (found activation epoch {})",
            entries[0].activation_epoch
        );
        for pair in entries.windows(2) {
            anyhow::ensure!(
                pair[0].activation_epoch < pair[1].activation_epoch,
                "schedule activation epochs must be strictly increasing ({} then {})",
                pair[0].activation_epoch,
                pair[1].activation_epoch
            );
        }
        for entry in &entries {
            anyhow::ensure!(
                !entry.committee.is_empty(),
                "schedule entry at epoch {} has an empty committee",
                entry.activation_epoch
            );
        }
        Ok(Self { entries })
    }

    /// A schedule with a single committee holding every epoch — the static-set
    /// deployment, and the shape the plain `consensus.validators` config sugars to.
    pub fn single(committee: BiMap<PublicKey, <MinPk as Variant>::Public>) -> anyhow::Result<Self> {
        Self::new(vec![ScheduleEntry {
            activation_epoch: 0,
            committee,
        }])
    }

    /// The entry holding `epoch`: the one with the highest activation at or below it.
    pub fn entry_for(&self, epoch: Epoch) -> &ScheduleEntry {
        let position = self
            .entries
            .partition_point(|entry| entry.activation_epoch <= epoch.get());
        // Validation pinned entries[0].activation_epoch == 0, so position >= 1.
        &self.entries[position - 1]
    }

    /// Is this network identity a committee member for `epoch`?
    pub fn is_member(&self, epoch: Epoch, identity: &PublicKey) -> bool {
        self.entry_for(epoch)
            .committee
            .get_value(identity)
            .is_some()
    }

    /// Does this network identity appear in *any* entry? A node whose key is in no
    /// committee, past or scheduled, has no business running with consensus
    /// enabled — the startup guard asks exactly this.
    pub fn member_of_any(&self, identity: &PublicKey) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.committee.get_value(identity).is_some())
    }

    /// All entries, in activation order (the node derives its p2p address book and
    /// transition records from these).
    pub fn entries(&self) -> &[ScheduleEntry] {
        &self.entries
    }
}

/// Per-epoch scheme provider over a [`CommitteeSchedule`].
///
/// For every epoch it produces a scheme over that epoch's committee: a *signer*
/// scheme when the configured BLS key belongs to a member, a *verifier-only*
/// scheme otherwise. Schemes are cached per schedule entry (not per epoch —
/// thousands of epochs share one committee), so repeated lookups are a map read.
///
/// Cloning shares the cache; the provider is handed to the marshal (which
/// verifies certificates from arbitrary historical epochs during backfill), to
/// the engine factory (which needs the signer for the epoch it runs), and to the
/// tip scout (which verifies finalizations from epochs this validator is not
/// running).
#[derive(Clone)]
pub struct ScheduledSchemeProvider {
    inner: Arc<ProviderInner>,
}

struct ProviderInner {
    /// The signing namespace (protocol-versioned); every epoch's scheme signs and
    /// verifies under the same domain separation.
    namespace: Vec<u8>,
    schedule: CommitteeSchedule,
    /// `None` builds verifier-only providers (observers, tooling).
    bls_key: Option<group::Private>,
    /// Keyed by the schedule entry's activation epoch.
    cache: Mutex<BTreeMap<u64, Arc<Scheme>>>,
}

impl ScheduledSchemeProvider {
    pub fn new(
        namespace: Vec<u8>,
        schedule: CommitteeSchedule,
        bls_key: Option<group::Private>,
    ) -> Self {
        Self {
            inner: Arc::new(ProviderInner {
                namespace,
                schedule,
                bls_key,
                cache: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    pub fn schedule(&self) -> &CommitteeSchedule {
        &self.inner.schedule
    }

    /// The scheme for `epoch` — signer if this validator is in that epoch's
    /// committee, verifier otherwise. Total: every epoch resolves.
    pub fn scheme_for(&self, epoch: Epoch) -> Arc<Scheme> {
        let entry = self.inner.schedule.entry_for(epoch);
        let mut cache = self.inner.cache.lock().unwrap();
        if let Some(scheme) = cache.get(&entry.activation_epoch) {
            return scheme.clone();
        }
        // Try to be a signer for this committee; fall back to verifier when the
        // key is not a member (or no key was configured at all). `signer` returning
        // `None` is how the scheme reports non-membership — not an error here:
        // being outside some epoch's committee is exactly what a schedule expresses.
        let scheme = self
            .inner
            .bls_key
            .clone()
            .and_then(|key| Scheme::signer(&self.inner.namespace, entry.committee.clone(), key))
            .unwrap_or_else(|| Scheme::verifier(&self.inner.namespace, entry.committee.clone()));
        let scheme = Arc::new(scheme);
        cache.insert(entry.activation_epoch, scheme.clone());
        scheme
    }
}

impl commonware_cryptography::certificate::Provider for ScheduledSchemeProvider {
    type Scope = Epoch;
    type Scheme = Scheme;

    fn scoped(&self, scope: Epoch) -> Option<Arc<Scheme>> {
        Some(self.scheme_for(scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_codec::DecodeExt as _;
    use commonware_cryptography::certificate::Scheme as _;
    use commonware_cryptography::{Signer as _, ed25519};
    use commonware_utils::TryFromIterator as _;
    use rand08::rngs::StdRng;
    use rand08::{RngCore as _, SeedableRng as _};

    /// Deterministic keys from seeded bytes — the same way deployment tooling mints
    /// them. (BLS scalars must be canonical, hence the retry loop.)
    fn keys(n: usize, rng: &mut StdRng) -> Vec<(ed25519::PrivateKey, group::Private)> {
        (0..n)
            .map(|_| {
                let mut seed = [0u8; 32];
                rng.fill_bytes(&mut seed);
                let network =
                    ed25519::PrivateKey::decode(seed.as_slice()).expect("32 bytes are a seed");
                let bls = loop {
                    let mut bytes = [0u8; 32];
                    rng.fill_bytes(&mut bytes);
                    if let Ok(key) = group::Private::decode(bytes.as_slice()) {
                        break key;
                    }
                };
                (network, bls)
            })
            .collect()
    }

    fn committee(
        members: &[(ed25519::PrivateKey, group::Private)],
    ) -> BiMap<PublicKey, <MinPk as Variant>::Public> {
        use commonware_cryptography::bls12381::primitives::ops::compute_public;
        BiMap::try_from_iter(
            members
                .iter()
                .map(|(network, bls)| (network.public_key(), compute_public::<MinPk>(bls))),
        )
        .expect("distinct keys")
    }

    #[test]
    fn schedule_selects_the_highest_activation_at_or_below() {
        let mut rng = StdRng::seed_from_u64(7);
        let all = keys(3, &mut rng);
        let schedule = CommitteeSchedule::new(vec![
            ScheduleEntry {
                activation_epoch: 0,
                committee: committee(&all[..2]),
            },
            ScheduleEntry {
                activation_epoch: 5,
                committee: committee(&all[..3]),
            },
        ])
        .expect("valid");

        assert_eq!(schedule.entry_for(Epoch::new(0)).activation_epoch, 0);
        assert_eq!(schedule.entry_for(Epoch::new(4)).activation_epoch, 0);
        assert_eq!(schedule.entry_for(Epoch::new(5)).activation_epoch, 5);
        assert_eq!(
            schedule.entry_for(Epoch::new(1_000_000)).activation_epoch,
            5
        );
    }

    #[test]
    fn schedule_validation_rejects_bad_shapes() {
        let mut rng = StdRng::seed_from_u64(8);
        let all = keys(2, &mut rng);

        assert!(CommitteeSchedule::new(vec![]).is_err(), "empty schedule");
        assert!(
            CommitteeSchedule::new(vec![ScheduleEntry {
                activation_epoch: 3,
                committee: committee(&all),
            }])
            .is_err(),
            "first entry must cover epoch 0"
        );
        assert!(
            CommitteeSchedule::new(vec![
                ScheduleEntry {
                    activation_epoch: 0,
                    committee: committee(&all),
                },
                ScheduleEntry {
                    activation_epoch: 0,
                    committee: committee(&all),
                },
            ])
            .is_err(),
            "activations must strictly increase"
        );
    }

    #[test]
    fn provider_returns_signer_inside_the_committee_and_verifier_outside() {
        let mut rng = StdRng::seed_from_u64(9);
        let all = keys(3, &mut rng);
        // Validator 2 joins at epoch 5 and validator 0 leaves.
        let schedule = CommitteeSchedule::new(vec![
            ScheduleEntry {
                activation_epoch: 0,
                committee: committee(&all[..2]),
            },
            ScheduleEntry {
                activation_epoch: 5,
                committee: committee(&all[1..3]),
            },
        ])
        .expect("valid");

        let provider = ScheduledSchemeProvider::new(
            b"test-namespace".to_vec(),
            schedule.clone(),
            Some(all[0].1.clone()),
        );
        assert!(
            provider.scheme_for(Epoch::new(0)).me().is_some(),
            "validator 0 signs in its epochs"
        );
        assert!(
            provider.scheme_for(Epoch::new(5)).me().is_none(),
            "validator 0 is a verifier after leaving"
        );

        let joiner = ScheduledSchemeProvider::new(
            b"test-namespace".to_vec(),
            schedule.clone(),
            Some(all[2].1.clone()),
        );
        assert!(joiner.scheme_for(Epoch::new(4)).me().is_none());
        assert!(joiner.scheme_for(Epoch::new(5)).me().is_some());

        let observer = ScheduledSchemeProvider::new(b"test-namespace".to_vec(), schedule, None);
        assert!(observer.scheme_for(Epoch::new(0)).me().is_none());

        // Membership questions agree with the schemes.
        use commonware_cryptography::Signer as _;
        assert!(schedule_is_consistent(&provider, &all[0].0.public_key()));
        fn schedule_is_consistent(
            provider: &ScheduledSchemeProvider,
            identity: &PublicKey,
        ) -> bool {
            let schedule = provider.schedule();
            schedule.is_member(Epoch::new(0), identity)
                && !schedule.is_member(Epoch::new(5), identity)
                && schedule.member_of_any(identity)
        }
    }

    #[test]
    fn provider_caches_by_entry_not_by_epoch() {
        let mut rng = StdRng::seed_from_u64(10);
        let all = keys(2, &mut rng);
        let provider = ScheduledSchemeProvider::new(
            b"test-namespace".to_vec(),
            CommitteeSchedule::single(committee(&all)).expect("valid"),
            Some(all[0].1.clone()),
        );
        let a = provider.scheme_for(Epoch::new(1));
        let b = provider.scheme_for(Epoch::new(999));
        assert!(Arc::ptr_eq(&a, &b), "same entry, same cached scheme");
    }
}
