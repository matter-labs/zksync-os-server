//! The observer shape at DST level: an honest node on the consensus network that is
//! never a committee member. It runs the full stack — marshal, backfill, the tip
//! scout, ordered commits — but the membership-gated rotation starts no engine for
//! it, ever. Everything it believes about the chain comes from verifying finality
//! certificates against the committee schedule; no single peer is its trust root.
//!
//! This is the deterministic pin for the node's `consensus.role = observer` (the
//! production observer is exactly this shape plus node-side config guards, an
//! admission list, and RPC transaction forwarding — wiring the L3 covers). The
//! nearby reconfig scenarios cover *transient* non-membership (scheduled out, or
//! not yet activated); this one pins the permanent case, across a committee change.

use std::num::NonZeroU64;
use std::time::Duration;
use zksync_os_consensus_sim::{
    Behavior, EraOptions, MockExecution, SimCluster, links, run_scenario,
};

const EPOCH_LENGTH: u64 = 8;

fn short_epochs() -> zksync_os_consensus_sim::StackTuner {
    std::sync::Arc::new(|config| {
        config.epoch_length = NonZeroU64::new(EPOCH_LENGTH).expect("nonzero");
    })
}

/// A never-member follows from genesis, through a committee change, and its view
/// of finality spans both committees' certificates.
#[test]
fn never_member_follows_from_genesis_and_through_reconfig() {
    run_scenario(
        "observer_follows_through_reconfig",
        0..3,
        Duration::from_secs(600),
        |context| async move {
            let behaviors = vec![Behavior::Honest; 5];
            // Validator 4 appears in NO schedule entry — a pure observer. The
            // committee itself shrinks 4 → 3 at epoch 2, so the observer must
            // verify certificates from two different committees to keep up.
            let schedule = vec![(0, vec![0, 1, 2, 3]), (2, vec![0, 1, 2])];
            let mut cluster = SimCluster::start_era(
                context,
                &behaviors,
                links::healthy(),
                |_index, _context| MockExecution::new(),
                EraOptions {
                    stack_tuner: short_epochs(),
                    schedule,
                    ..EraOptions::default()
                },
            )
            .await;

            // Everyone — the observer included — commits the same chain across
            // the boundary at epoch 2 (height 16) and beyond.
            cluster
                .wait_for_committed_height_all(3 * EPOCH_LENGTH + 2)
                .await;
            cluster.assert_committed_chains_agree(3 * EPOCH_LENGTH);

            // The observer's belief really spans both regimes: it verified (and
            // recorded) finalizations from the epoch-0 committee and from the
            // shrunk committee — the schedule, not any serving peer, is what it
            // trusted across the change.
            let observed = cluster.validators[4].activity.finalizations_newest_first();
            let epochs: std::collections::BTreeSet<u64> = observed
                .iter()
                .map(|finalization| finalization.round().epoch().get())
                .collect();
            assert!(
                epochs.contains(&0) && epochs.iter().any(|&epoch| epoch >= 2),
                "observer's verified finalizations should span the committee change; \
                 saw epochs {epochs:?}"
            );

            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}
