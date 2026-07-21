//! The registry derivation driver: turns on-chain validator-registry state into
//! per-epoch committee decisions, durably and deterministically.
//!
//! The committee for epoch `T` is derived from chain state at a fixed height —
//! the last block of epoch `T−2` ([`lookahead_height`]), one full epoch of
//! lookahead. Because the height is fixed, the outcome is a pure function of
//! finalized chain state: every honest node computes identical bytes, no matter
//! when it computes them. The driver walks target epochs strictly in order, and
//! for each one:
//!
//! 1. waits until the node has durably applied the lookahead height;
//! 2. asks the [`DerivationSource`] to read and validate the registry there;
//! 3. resolves the committee in effect — the registry's on success, the carried
//!    previous one when the registry has nothing usable (*refuse to rotate*:
//!    a broken registry blocks committee changes, never the chain);
//! 4. persists the outcome to the [`DerivationLedger`] (first-observed-wins; the
//!    durable trail restarts and floor rebuilds replay instead of re-deriving);
//! 5. appends it to the [`CommitteeSource`] and reports the observation (status
//!    surface, drift comparison against the config schedule).
//!
//! The driver is mode-blind: whether recorded derivations *govern* consensus is
//! the [`CommitteeSource`]'s precedence question (`registry_from`), not the
//! driver's. Shadow mode is this exact driver with a source that never consults
//! its recordings.

use crate::schedule::{Committee, CommitteeSource};
use commonware_runtime::{Clock, Spawner};
use std::num::NonZeroU64;
use std::time::Duration;
use tracing::{info, warn};

/// What one registry read at a lookahead height yielded, before carry semantics.
#[derive(Debug)]
pub enum RegistryReading {
    /// A valid, fully validated schedule entry applies to the epoch.
    Committee(Committee),
    /// The registry is readable and valid but holds no entry applicable to the
    /// epoch (deployed-but-unpopulated, or populated only for later epochs).
    NoEntry,
    /// The registry refused validation; the reason is for logs and status (the
    /// durable record keeps only the outcome kind — reasons are re-derivable
    /// from chain state, and encoding them would freeze wording into a format).
    Refused(String),
}

/// One derivation attempt's result, separating chain facts from local problems.
#[derive(Debug)]
pub enum DerivationAttempt {
    /// Chain state at the lookahead height is not readable here and now (not yet
    /// applied despite the height gate — a race with pruning, a backend hiccup).
    /// Not a chain fact: never recorded, retried on the next tick.
    Unavailable,
    /// The registry was read from chain state; the reading is a chain fact.
    Reading(RegistryReading),
}

/// Reads the registry out of chain state. The node implements this over its
/// state backend + the registry contract parser; simulations implement it over
/// manufactured state.
pub trait DerivationSource: Send + 'static {
    /// One attempt to derive the committee for `epoch` from chain state at
    /// `lookahead_height` (chain-absolute).
    fn derive(&mut self, epoch: u64, lookahead_height: u64) -> DerivationAttempt;
}

/// How a recorded derivation decided its committee — the wire outcome, mirrored
/// here so this crate stays free of the wire dependency (conversion happens at
/// the ledger implementation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedOutcome {
    Derived,
    CarriedNoEntry,
    CarriedRefused,
}

/// One derivation as recorded: the committee in effect for the epoch and how it
/// was decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedDerivation {
    pub epoch: u64,
    pub lookahead_height: u64,
    pub outcome: RecordedOutcome,
    pub committee: Committee,
}

/// The durable derivation trail. Implementations persist records
/// first-observed-wins (re-recording an epoch leaves the original untouched).
pub trait DerivationLedger: Send + 'static {
    /// Every recorded derivation, ascending by epoch.
    fn load(&self) -> anyhow::Result<Vec<RecordedDerivation>>;
    /// Persists a record. Returns whether it was written (`false`: one already
    /// existed for the epoch).
    fn record(&self, record: &RecordedDerivation) -> anyhow::Result<bool>;
}

/// What the driver reports after each completed derivation — the status
/// surface's raw material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryObservation {
    pub epoch: u64,
    pub lookahead_height: u64,
    pub outcome: RecordedOutcome,
    /// Human-readable refusal reason, on `CarriedRefused` only.
    pub refusal: Option<String>,
    /// Whether the committee in effect equals the config schedule's answer for
    /// the epoch — the shadow-mode drift signal.
    pub matches_config: bool,
    /// The committee in effect for the epoch under this derivation.
    pub committee: Committee,
}

/// How many epochs of lookahead a derivation gets: epoch `T` derives from state
/// at the last block of epoch `T − LOOKAHEAD_EPOCHS`. One full epoch of margin —
/// the height is long finalized *and applied* by the time the committee matters,
/// and incoming validators get a full epoch of warning to connect. A
/// committee-uniform protocol constant: it joins the chain fingerprint, and
/// changing it is a flag-day event.
pub const LOOKAHEAD_EPOCHS: u64 = 2;

/// The chain-absolute height whose state decides epoch `target`'s committee:
/// the last block of epoch `target − LOOKAHEAD_EPOCHS` (see
/// [`LOOKAHEAD_EPOCHS`]). The first epochs of an era have no such block; they
/// derive from the era anchor itself (a genesis-seeded registry is readable
/// there).
pub fn lookahead_height(era_anchor: u64, epoch_length: NonZeroU64, target: u64) -> u64 {
    if target < LOOKAHEAD_EPOCHS {
        return era_anchor;
    }
    // Era-relative: epoch K spans [K·len, (K+1)·len − 1], so the last block of
    // epoch (target − LOOKAHEAD_EPOCHS) is (target − LOOKAHEAD_EPOCHS + 1)·len − 1.
    era_anchor + (target - LOOKAHEAD_EPOCHS + 1) * epoch_length.get() - 1
}

/// The first epoch a driver starting fresh *now* can serve: the smallest target
/// whose lookahead height is at or above the given applied height — boundaries
/// already in the past would need historical state that may be pruned; shadow
/// coverage simply starts at the next boundary. (Contract mode instead resumes
/// from its ledger and the flip epoch — see the node wiring.)
pub fn first_live_target(era_anchor: u64, epoch_length: NonZeroU64, applied: u64) -> u64 {
    let mut target = 0;
    while lookahead_height(era_anchor, epoch_length, target) < applied {
        target += 1;
    }
    target
}

/// Replays a loaded ledger into the committee source. Returns the newest
/// replayed epoch, if any — the driver resumes right after it.
pub fn replay_ledger(source: &CommitteeSource, records: &[RecordedDerivation]) -> Option<u64> {
    let mut newest = None;
    for record in records {
        source.record_derivation(record.epoch, record.committee.clone());
        newest = Some(newest.map_or(record.epoch, |seen: u64| seen.max(record.epoch)));
    }
    newest
}

/// How often the driver re-checks the applied height against the next lookahead
/// boundary. Boundaries are hours apart in production; the interval only bounds
/// how soon after the boundary the derivation lands.
const DERIVATION_POLL: Duration = Duration::from_millis(250);

/// Runs the derivation loop (see the module docs). Spawn once per node with
/// consensus enabled and a registry configured; runs for the validator's
/// lifetime.
///
/// `applied` reports the chain-absolute height durably applied by the node's
/// pipeline (`None` before the first block). `initial_target` is the first
/// epoch to derive — the caller computes it from the replayed ledger and the
/// mode (see [`replay_ledger`] / [`first_live_target`]).
#[allow(clippy::too_many_arguments)]
pub async fn run_registry_derivation<R, S, L>(
    context: R,
    era_anchor: u64,
    epoch_length: NonZeroU64,
    initial_target: u64,
    applied: impl Fn() -> Option<u64> + Send + 'static,
    mut source: S,
    ledger: L,
    committees: CommitteeSource,
    mut observe: impl FnMut(RegistryObservation) + Send + 'static,
) where
    R: Clock + Spawner,
    S: DerivationSource,
    L: DerivationLedger,
{
    use futures::FutureExt as _;
    let mut target = initial_target;
    // Unavailability is worth one warning per target, not one per tick.
    let mut announced_unavailable = false;
    let mut stopped = context.stopped();
    loop {
        if (&mut stopped).now_or_never().is_some() {
            return;
        }
        let boundary = lookahead_height(era_anchor, epoch_length, target);
        if applied().is_none_or(|height| height < boundary) {
            context.sleep(DERIVATION_POLL).await;
            continue;
        }
        let reading = match source.derive(target, boundary) {
            DerivationAttempt::Unavailable => {
                if !announced_unavailable {
                    warn!(
                        epoch = target,
                        lookahead_height = boundary,
                        "chain state at the lookahead height is not readable; the \
                         derivation will retry (if this persists, the height was \
                         pruned before the derivation ran — the recorded trail on \
                         other nodes still covers it)"
                    );
                    announced_unavailable = true;
                }
                context.sleep(DERIVATION_POLL).await;
                continue;
            }
            DerivationAttempt::Reading(reading) => reading,
        };
        announced_unavailable = false;

        // Carry semantics: with nothing usable from the registry, the previous
        // committee stays — the newest recorded derivation, or (before any) the
        // config schedule's answer. Rotation is what a broken registry blocks;
        // the chain is not.
        //
        // The carry is fenced at the flip: a governed epoch may only chain
        // from *governed* recordings. Sub-flip (shadow-era) trails differ per
        // node — a fresh joiner has none, a veteran has some, a veteran that
        // slept through boundaries is missing exactly those (shadow skips
        // them) — so carrying one into governance would make the committee a
        // function of node-local history and split it. Before the first
        // governed recording, the config schedule's entry (consensus-uniform,
        // the documented mirror) is the carry base on every node.
        let config_committee = committees
            .config_schedule()
            .entry_for(crate::types::Epoch::new(target))
            .committee
            .clone();
        let carry_floor = committees.registry_from().unwrap_or(0);
        let carried = || {
            committees
                .last_derived_in(carry_floor, target)
                .unwrap_or_else(|| config_committee.clone())
        };
        let (outcome, refusal, committee) = match reading {
            RegistryReading::Committee(committee) => (RecordedOutcome::Derived, None, committee),
            RegistryReading::NoEntry => (RecordedOutcome::CarriedNoEntry, None, carried()),
            RegistryReading::Refused(reason) => {
                (RecordedOutcome::CarriedRefused, Some(reason), carried())
            }
        };
        let record = RecordedDerivation {
            epoch: target,
            lookahead_height: boundary,
            outcome,
            committee,
        };
        // The durable trail gates the in-memory answer: a derivation the ledger
        // cannot hold would evaporate on restart, so a persist failure retries
        // rather than advancing (loudly — this is a disk problem, not a chain
        // fact).
        match ledger.record(&record) {
            Ok(_) => {}
            Err(err) => {
                warn!(
                    ?err,
                    epoch = target,
                    "failed to persist a registry derivation; retrying"
                );
                context.sleep(DERIVATION_POLL).await;
                continue;
            }
        }
        committees.record_derivation(target, record.committee.clone());

        let matches_config = record.committee == config_committee;
        let observation = RegistryObservation {
            epoch: target,
            lookahead_height: boundary,
            outcome,
            refusal: refusal.clone(),
            matches_config,
            committee: record.committee.clone(),
        };
        match (&outcome, matches_config) {
            (RecordedOutcome::Derived, true) => info!(
                epoch = target,
                lookahead_height = boundary,
                committee_size = observation.committee.len(),
                "registry derivation matches the config schedule"
            ),
            (RecordedOutcome::Derived, false) => warn!(
                epoch = target,
                lookahead_height = boundary,
                committee_size = observation.committee.len(),
                "REGISTRY DRIFT: the registry derives a committee the config \
                 schedule does not"
            ),
            (RecordedOutcome::CarriedNoEntry, matches) => info!(
                epoch = target,
                lookahead_height = boundary,
                matches_config = matches,
                "registry holds no applicable schedule entry; carrying the \
                 previous committee"
            ),
            (RecordedOutcome::CarriedRefused, _) => warn!(
                epoch = target,
                lookahead_height = boundary,
                refusal = refusal.as_deref().unwrap_or(""),
                "REGISTRY REFUSED: derivation carried the previous committee \
                 (rotation via the registry is blocked until governance fixes it)"
            ),
        }
        observe(observation);
        target += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_utils::NZU64;

    #[test]
    fn lookahead_is_the_last_block_of_the_epoch_two_before() {
        let len = NZU64!(100);
        // Epoch 2 derives from the last block of epoch 0 (heights 0..=99).
        assert_eq!(lookahead_height(0, len, 2), 99);
        assert_eq!(lookahead_height(0, len, 3), 199);
        // The first two epochs anchor at the era genesis.
        assert_eq!(lookahead_height(0, len, 0), 0);
        assert_eq!(lookahead_height(0, len, 1), 0);
        // A migrated era offsets everything by its anchor.
        assert_eq!(lookahead_height(500, len, 2), 599);
        assert_eq!(lookahead_height(500, len, 0), 500);
    }

    #[test]
    fn first_live_target_skips_boundaries_already_in_the_past() {
        let len = NZU64!(100);
        // Fresh chain, nothing applied: start from the beginning.
        assert_eq!(first_live_target(0, len, 0), 0);
        // Applied halfway through epoch 3 (height 350): boundaries at 0, 99,
        // 199, 299 have passed; the first future one is 399 = epoch 5's.
        assert_eq!(first_live_target(0, len, 350), 5);
        // Exactly on a boundary still counts it as live.
        assert_eq!(first_live_target(0, len, 199), 3);
    }
}
