//! Records raw consensus activity for test assertions.

use commonware_consensus::Reporter;
use commonware_consensus::simplex::types::Activity;
use commonware_consensus::simplex::types::Attributable;
use commonware_cryptography::sha256::Digest;
use std::collections::BTreeSet;
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
}

impl Reporter for ActivityLog {
    type Activity = Activity<Scheme, Digest>;

    async fn report(&mut self, activity: Self::Activity) {
        let culprit = match &activity {
            Activity::ConflictingNotarize(evidence) => Some(evidence.signer()),
            Activity::ConflictingFinalize(evidence) => Some(evidence.signer()),
            Activity::NullifyFinalize(evidence) => Some(evidence.signer()),
            // Votes and certificates flow through here too; the committed chains (via
            // MockExecution) are the more meaningful signal for those.
            _ => None,
        };
        if let Some(culprit) = culprit {
            let mut inner = self.inner.lock().unwrap();
            inner.faults += 1;
            inner.culprits.insert(culprit.get());
        }
    }
}
