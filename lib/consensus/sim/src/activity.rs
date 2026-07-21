//! Records raw consensus activity for test assertions.

use commonware_actor::Feedback;
use commonware_consensus::Reporter;
use commonware_consensus::simplex::types::Attributable;
use commonware_consensus::simplex::types::{Activity, Finalization};
use commonware_cryptography::sha256::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use zksync_os_consensus_core::types::Scheme;

/// Plugged into the consensus engine as an extra observer.
///
/// Tests use it two ways: honest clusters assert that *no* fault evidence ever appears,
/// and byzantine scenarios assert that evidence appears and points at exactly the
/// misbehaving validator (fault evidence is self-incriminating — it consists of the
/// culprit's own conflicting signatures, so honest validators can never be framed).
#[derive(Clone, Default)]
pub struct ActivityLog {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    faults: usize,
    /// Committee positions of validators that produced fault evidence.
    culprits: BTreeSet<u32>,
    /// Every finalization observed, by round — the sim stand-in for the node's
    /// finality store (which retains raw finalizations for floor-started restarts).
    finalizations: BTreeMap<(u64, u64), Finalization<Scheme, Digest>>,
}

impl ActivityLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of Byzantine-fault evidences observed (test probe). Zero in honest clusters.
    pub fn faults(&self) -> usize {
        self.inner.lock().unwrap().faults
    }

    /// Committee positions all observed fault evidence points at (test probe).
    pub fn fault_culprits(&self) -> BTreeSet<u32> {
        self.inner.lock().unwrap().culprits.clone()
    }

    /// All observed finalizations, newest round first — floor candidates for a
    /// floor-started restart (see [`crate::SimCluster::floor_at_or_below`]).
    pub fn finalizations_newest_first(&self) -> Vec<Finalization<Scheme, Digest>> {
        self.inner
            .lock()
            .unwrap()
            .finalizations
            .values()
            .rev()
            .cloned()
            .collect()
    }
}

impl Reporter for ActivityLog {
    type Activity = Activity<Scheme, Digest>;

    fn report(&mut self, activity: Self::Activity) -> Feedback {
        match &activity {
            Activity::ConflictingNotarize(evidence) => self.record_fault(evidence.signer().get()),
            Activity::ConflictingFinalize(evidence) => self.record_fault(evidence.signer().get()),
            Activity::NullifyFinalize(evidence) => self.record_fault(evidence.signer().get()),
            Activity::Finalization(finalization) => {
                let round = finalization.round();
                self.inner.lock().unwrap().finalizations.insert(
                    (round.epoch().get(), round.view().get()),
                    finalization.clone(),
                );
            }
            // Individual votes and other certificates flow through here too; the
            // committed chains (via MockExecution) are the more meaningful signal.
            _ => {}
        }
        Feedback::Ok
    }
}

impl ActivityLog {
    fn record_fault(&self, culprit: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.faults += 1;
        inner.culprits.insert(culprit);
    }
}
