//! Records raw consensus activity for test assertions.

use commonware_consensus::Reporter;
use commonware_consensus::simplex::types::Activity;
use commonware_cryptography::sha256::Digest;
use std::sync::{Arc, Mutex};
use zksync_os_consensus_core::types::Scheme;

/// Plugged into the consensus engine as an extra observer. Tests use it to assert the
/// cluster stayed clean: honest validators must never produce Byzantine fault evidence
/// (conflicting votes for one view), no matter what the network does to them.
#[derive(Clone, Default)]
pub struct ActivityLog {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    faults: usize,
}

impl ActivityLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of Byzantine-fault evidences observed (test probe). Zero in honest clusters.
    pub fn faults(&self) -> usize {
        self.inner.lock().unwrap().faults
    }
}

impl Reporter for ActivityLog {
    type Activity = Activity<Scheme, Digest>;

    async fn report(&mut self, activity: Self::Activity) {
        match activity {
            Activity::ConflictingNotarize(_)
            | Activity::ConflictingFinalize(_)
            | Activity::NullifyFinalize(_) => {
                self.inner.lock().unwrap().faults += 1;
            }
            // Votes and certificates flow through here too; the committed chains (via
            // MockExecution) are the more meaningful signal for those.
            _ => {}
        }
    }
}
