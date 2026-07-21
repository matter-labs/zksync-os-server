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
use std::sync::{Arc, Mutex, RwLock};

/// One committee: the ordered (network identity → consensus key) map. Order is
/// part of the agreement — participant indices in certificate bitmaps follow it.
pub type Committee = BiMap<PublicKey, <MinPk as Variant>::Public>;

/// One schedule entry: the committee that holds every epoch from
/// `activation_epoch` until a later entry supersedes it.
#[derive(Clone, Debug)]
pub struct ScheduleEntry {
    pub activation_epoch: u64,
    /// The ordered (network identity → consensus key) map. Order is part of the
    /// agreement: participant indices in certificate bitmaps follow it, so every
    /// validator must configure the same members in the same order.
    pub committee: Committee,
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

/// Which authority decides an epoch's committee, and — for the provider's cache —
/// under what identity the answer may be memoized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Governance {
    /// A config schedule entry governs the epoch; identified by its activation
    /// epoch (many epochs share one entry).
    Config { activation_epoch: u64 },
    /// The registry's recorded derivation for exactly this epoch governs.
    Derived { epoch: u64 },
    /// The registry governs the epoch but its derivation has not been recorded
    /// yet. The committee returned alongside is the latest known one — good
    /// enough to *attempt* verifying an early-arriving certificate (a failure is
    /// dropped, never a fault), never good enough to start an engine with or to
    /// cache: the real answer may still arrive and differ.
    Unsettled,
}

/// The committee authority for every epoch: the config schedule, optionally
/// handing over to registry-derived committees from a flip epoch on.
///
/// This is the one object the whole stack asks "who holds epoch E" — the scheme
/// provider (and through it marshal, engines, the tip scout), the epoch rotation
/// (which refuses to start an engine for an epoch whose answer is not settled),
/// and the custody-record observer. Derived committees are appended at runtime by
/// the registry derivation driver ([`crate::registry`]); appending never blocks
/// readers of other epochs.
///
/// Precedence: with no flip epoch the registry never governs anything (the
/// `schedule` and `shadow` modes — shadow derivations are still appended here,
/// they are just never consulted). With a flip epoch (`config_shadow` mode),
/// epochs before it follow config and epochs at or after it follow the recorded
/// derivation. Config entries at or after the flip do *not* override the
/// registry — they are the expected mirror of it: the drift comparison's
/// reference, the p2p address book's source, and the fallback while an epoch's
/// derivation is pending. Recovery from a broken registry is a mode change
/// (back to `schedule`/`shadow`, where config governs), not a config-entry
/// override — an override that operators must also use for routine mirroring
/// would be impossible to reserve for emergencies.
#[derive(Clone)]
pub struct CommitteeSource {
    inner: Arc<SourceInner>,
}

struct SourceInner {
    config: CommitteeSchedule,
    /// From this epoch on, recorded derivations govern (`config_shadow` mode).
    /// `None`: config governs everything, forever.
    registry_from: Option<u64>,
    /// Registry-derived committees, one per epoch, dense from the driver's first
    /// target onward (a carried-forward committee is recorded per epoch too, so
    /// presence means "the derivation for this epoch completed").
    derived: RwLock<BTreeMap<u64, Committee>>,
}

impl CommitteeSource {
    /// A source where the config schedule governs every epoch (the `schedule`
    /// and `shadow` modes).
    pub fn from_config(config: CommitteeSchedule) -> Self {
        Self {
            inner: Arc::new(SourceInner {
                config,
                registry_from: None,
                derived: RwLock::new(BTreeMap::new()),
            }),
        }
    }

    /// A source where recorded derivations govern from `registry_from` on
    /// (`config_shadow` mode). The config schedule's epoch-0 totality guarantees
    /// a fallback committee below the flip.
    pub fn with_registry_from(config: CommitteeSchedule, registry_from: u64) -> Self {
        Self {
            inner: Arc::new(SourceInner {
                config,
                registry_from: Some(registry_from),
                derived: RwLock::new(BTreeMap::new()),
            }),
        }
    }

    /// The config schedule (the address book, guards, fingerprint, and drift
    /// comparison read it; epoch resolution goes through [`Self::resolve`]).
    pub fn config_schedule(&self) -> &CommitteeSchedule {
        &self.inner.config
    }

    /// The epoch derivations start governing at, if any (`config_shadow` mode).
    pub fn registry_from(&self) -> Option<u64> {
        self.inner.registry_from
    }

    /// The committee for `epoch` and who decided it. Total — every epoch gets a
    /// committee — but an [`Governance::Unsettled`] answer is a placeholder (see
    /// its docs for what callers may do with one).
    pub fn resolve(&self, epoch: Epoch) -> (Committee, Governance) {
        let entry = self.inner.config.entry_for(epoch);
        let config_answer = || {
            (
                entry.committee.clone(),
                Governance::Config {
                    activation_epoch: entry.activation_epoch,
                },
            )
        };
        let Some(flip) = self.inner.registry_from else {
            return config_answer();
        };
        if epoch.get() < flip {
            return config_answer();
        }
        let derived = self.inner.derived.read().unwrap();
        match derived.range(..=epoch.get()).next_back() {
            Some((&recorded, committee)) if recorded == epoch.get() => (
                committee.clone(),
                Governance::Derived { epoch: epoch.get() },
            ),
            // The derivation for this epoch is still pending: answer with the
            // latest known committee (the newest recording, or the config
            // schedule's answer before any exists — which may be a mirror entry
            // at or after the flip) so verification attempts have something to
            // try — but say so.
            Some((_, committee)) => (committee.clone(), Governance::Unsettled),
            None => (entry.committee.clone(), Governance::Unsettled),
        }
    }

    /// Whether `epoch`'s committee is decided — config-governed, or covered by a
    /// recorded derivation. The rotation refuses to start an engine for an
    /// unsettled epoch (the answer may still change); everything else treats
    /// unsettled as best-effort.
    pub fn settled_for(&self, epoch: Epoch) -> bool {
        !matches!(self.resolve(epoch).1, Governance::Unsettled)
    }

    /// Appends the registry's derivation for `epoch`. Idempotent; the first
    /// recording wins (matching the durable ledger's first-observed-wins rule),
    /// and a conflicting re-recording is loud — two different answers for one
    /// epoch on one node means the derivation was not the pure function of chain
    /// state it must be.
    pub fn record_derivation(&self, epoch: u64, committee: Committee) {
        let mut derived = self.inner.derived.write().unwrap();
        if let Some(existing) = derived.get(&epoch) {
            if *existing != committee {
                tracing::error!(
                    epoch,
                    "conflicting registry derivations for one epoch on one node; \
                     keeping the first"
                );
                debug_assert!(false, "conflicting registry derivations for epoch {epoch}");
            }
            return;
        }
        derived.insert(epoch, committee);
    }

    /// The newest recorded derivation with epoch in `[floor, before)`, if any —
    /// the "last known committee" a refused or empty derivation carries
    /// forward.
    ///
    /// The floor is the carry's authority boundary and it is deliberately a
    /// required argument: a governed epoch's carry must never chain from
    /// sub-flip (shadow-era) recordings, because *which* shadow recordings a
    /// node holds depends on its operational history — when it joined, which
    /// mode it ran before, which boundaries it slept through (shadow skips
    /// them) — and a consensus-critical answer derived from node-local
    /// history splits the committee. Below the floor, the config schedule
    /// (consensus-uniform by definition) is the only legitimate carry base.
    pub fn last_derived_in(&self, floor: u64, before: u64) -> Option<Committee> {
        let derived = self.inner.derived.read().unwrap();
        derived
            .range(floor..before)
            .next_back()
            .map(|(_, committee)| committee.clone())
    }

    /// The newest recorded derivation epoch, if any (the driver resumes after it).
    pub fn latest_derived_epoch(&self) -> Option<u64> {
        let derived = self.inner.derived.read().unwrap();
        derived.keys().next_back().copied()
    }

    /// Does this network identity appear in any config entry or any recorded
    /// derivation? The startup guard's question, now across both authorities.
    pub fn member_of_any(&self, identity: &PublicKey) -> bool {
        self.inner.config.member_of_any(identity)
            || self
                .inner
                .derived
                .read()
                .unwrap()
                .values()
                .any(|committee| committee.get_value(identity).is_some())
    }
}

/// Per-epoch scheme provider over a [`CommitteeSource`].
///
/// For every epoch it produces a scheme over that epoch's committee: a *signer*
/// scheme when the configured BLS key belongs to a member, a *verifier-only*
/// scheme otherwise. Schemes are cached per governing answer (a config entry
/// covers many epochs under one cache slot; a derived committee is per-epoch),
/// so repeated lookups are a map read. Unsettled answers are never cached — the
/// real committee may still arrive and differ.
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

/// Cache key: the governing answer's identity (see [`Governance`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SchemeKey {
    ConfigEntry(u64),
    DerivedEpoch(u64),
}

struct ProviderInner {
    /// The signing namespace (protocol-versioned); every epoch's scheme signs and
    /// verifies under the same domain separation.
    namespace: Vec<u8>,
    source: CommitteeSource,
    /// `None` builds verifier-only providers (observers, tooling).
    bls_key: Option<group::Private>,
    cache: Mutex<BTreeMap<SchemeKey, Arc<Scheme>>>,
}

impl ScheduledSchemeProvider {
    /// A provider over a plain config schedule (every existing deployment shape;
    /// registry-fed providers use [`Self::over_source`]).
    pub fn new(
        namespace: Vec<u8>,
        schedule: CommitteeSchedule,
        bls_key: Option<group::Private>,
    ) -> Self {
        Self::over_source(namespace, CommitteeSource::from_config(schedule), bls_key)
    }

    pub fn over_source(
        namespace: Vec<u8>,
        source: CommitteeSource,
        bls_key: Option<group::Private>,
    ) -> Self {
        Self {
            inner: Arc::new(ProviderInner {
                namespace,
                source,
                bls_key,
                cache: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// The config schedule behind this provider. Epoch resolution must go through
    /// [`Self::scheme_for`] / [`Self::source`] — under a registry flip the config
    /// schedule alone does not answer "who holds epoch E".
    pub fn schedule(&self) -> &CommitteeSchedule {
        self.inner.source.config_schedule()
    }

    /// The committee authority this provider reads (the derivation driver appends
    /// to it; observers and status read through it).
    pub fn source(&self) -> &CommitteeSource {
        &self.inner.source
    }

    /// Whether `epoch`'s committee is decided (see [`CommitteeSource::settled_for`]).
    pub fn settled_for(&self, epoch: Epoch) -> bool {
        self.inner.source.settled_for(epoch)
    }

    /// The scheme for `epoch` — signer if this validator is in that epoch's
    /// committee, verifier otherwise. Total: every epoch resolves, though an
    /// epoch whose registry derivation is still pending resolves to the latest
    /// known committee (uncached; see [`Governance::Unsettled`]).
    pub fn scheme_for(&self, epoch: Epoch) -> Arc<Scheme> {
        let (committee, governance) = self.inner.source.resolve(epoch);
        let key = match governance {
            Governance::Config { activation_epoch } => {
                Some(SchemeKey::ConfigEntry(activation_epoch))
            }
            Governance::Derived { epoch } => Some(SchemeKey::DerivedEpoch(epoch)),
            Governance::Unsettled => None,
        };
        if let Some(key) = key
            && let Some(scheme) = self.inner.cache.lock().unwrap().get(&key)
        {
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
            .and_then(|key| Scheme::signer(&self.inner.namespace, committee.clone(), key))
            .unwrap_or_else(|| Scheme::verifier(&self.inner.namespace, committee));
        let scheme = Arc::new(scheme);
        if let Some(key) = key {
            self.inner.cache.lock().unwrap().insert(key, scheme.clone());
        }
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
    fn source_precedence_covers_flip_mirror_and_carry() {
        let mut rng = StdRng::seed_from_u64(11);
        let all = keys(4, &mut rng);
        let config = CommitteeSchedule::new(vec![
            ScheduleEntry {
                activation_epoch: 0,
                committee: committee(&all[..2]),
            },
            // A mirror entry: config tracking a registry rotation at epoch 9.
            ScheduleEntry {
                activation_epoch: 9,
                committee: committee(&all[..3]),
            },
        ])
        .expect("valid");
        let source = CommitteeSource::with_registry_from(config, 4);

        // Below the flip: config governs, settled.
        let (c, governance) = source.resolve(Epoch::new(3));
        assert_eq!(
            governance,
            Governance::Config {
                activation_epoch: 0
            }
        );
        assert_eq!(c.len(), 2);
        assert!(source.settled_for(Epoch::new(3)));

        // At the flip with nothing derived: the config fallback answers, but the
        // epoch is not settled — no engine may start over it.
        let (c, governance) = source.resolve(Epoch::new(4));
        assert_eq!(governance, Governance::Unsettled);
        assert_eq!(c.len(), 2, "fallback is the last config committee");
        assert!(!source.settled_for(Epoch::new(4)));

        // With nothing derived, a mirror entry is the best available fallback
        // for its epochs — still unsettled (the derivation decides).
        let (c, governance) = source.resolve(Epoch::new(10));
        assert_eq!(governance, Governance::Unsettled);
        assert_eq!(c.len(), 3, "the mirror entry serves as the fallback");
        assert!(!source.settled_for(Epoch::new(10)));

        // A recorded derivation settles its epoch exactly.
        source.record_derivation(4, committee(&all[1..4]));
        let (c, governance) = source.resolve(Epoch::new(4));
        assert_eq!(governance, Governance::Derived { epoch: 4 });
        assert_eq!(c.len(), 3);
        assert!(source.settled_for(Epoch::new(4)));
        // The next epoch answers with the newest derivation but stays unsettled.
        let (c, governance) = source.resolve(Epoch::new(5));
        assert_eq!(governance, Governance::Unsettled);
        assert_eq!(c.len(), 3);

        // Mirror entries never override: the recorded derivation governs epoch 9
        // even though a config entry names it — the entry is the comparison
        // reference and address-book source, not an authority (recovery from a
        // broken registry is a mode change, not a config-entry override).
        source.record_derivation(9, committee(&all[..2]));
        let (c, governance) = source.resolve(Epoch::new(9));
        assert_eq!(governance, Governance::Derived { epoch: 9 });
        assert_eq!(c.len(), 2);
        let (c, governance) = source.resolve(Epoch::new(10));
        assert_eq!(governance, Governance::Unsettled);
        assert_eq!(c.len(), 2, "the newest derivation carries past its epoch");

        // Carry helpers: the newest derivation strictly below an epoch.
        assert_eq!(source.last_derived_in(0, 9).expect("epoch 4").len(), 3);
        assert!(source.last_derived_in(0, 4).is_none());
        // The floor fences the carry at an authority boundary: recordings
        // below it are invisible even when present.
        assert!(source.last_derived_in(5, 9).is_none());
        assert_eq!(source.latest_derived_epoch(), Some(9));
    }

    #[test]
    fn source_without_a_flip_never_consults_derivations() {
        let mut rng = StdRng::seed_from_u64(12);
        let all = keys(3, &mut rng);
        let source =
            CommitteeSource::from_config(CommitteeSchedule::single(committee(&all[..2])).unwrap());
        // The shadow driver records; resolution ignores it entirely.
        source.record_derivation(7, committee(&all[..3]));
        let (c, governance) = source.resolve(Epoch::new(100));
        assert_eq!(
            governance,
            Governance::Config {
                activation_epoch: 0
            }
        );
        assert_eq!(c.len(), 2);
        assert!(source.settled_for(Epoch::new(100)));
        // Membership spans both authorities regardless (the startup guard's view).
        use commonware_cryptography::Signer as _;
        assert!(source.member_of_any(&all[2].0.public_key()));
    }

    #[test]
    fn provider_over_a_source_serves_derived_committees_and_skips_unsettled_cache() {
        let mut rng = StdRng::seed_from_u64(13);
        let all = keys(3, &mut rng);
        let source = CommitteeSource::with_registry_from(
            CommitteeSchedule::single(committee(&all[..2])).unwrap(),
            2,
        );
        let provider = ScheduledSchemeProvider::over_source(
            b"test-namespace".to_vec(),
            source.clone(),
            Some(all[2].1.clone()),
        );

        // Unsettled: validator 2 is not in the fallback committee — and the
        // answer must not be cached, because the derivation may change it.
        assert!(provider.scheme_for(Epoch::new(5)).me().is_none());
        source.record_derivation(2, committee(&all[..2]));
        source.record_derivation(3, committee(&all[..3]));
        // The derivation for epoch 3 admits validator 2; a stale cache entry
        // would still say no.
        assert!(provider.scheme_for(Epoch::new(3)).me().is_some());
        // Settled per-epoch answers are cached per epoch.
        let a = provider.scheme_for(Epoch::new(3));
        let b = provider.scheme_for(Epoch::new(3));
        assert!(Arc::ptr_eq(&a, &b));
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
