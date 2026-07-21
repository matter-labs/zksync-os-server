//! Byzantine validator scenarios: one committee member actively attacks the protocol.
//!
//! What must hold, per attack:
//! - **Safety**: the honest validators keep committing one identical chain.
//! - **Detection**: honest validators record fault evidence, and all of it points at
//!   the attacker (evidence is the attacker's own conflicting signatures, so honest
//!   validators cannot be framed).
//! - **Containment**: the network layer blocks the attacker; no honest validator gets
//!   blocked by anyone.
//!
//! With 5 validators and quorum 4, the four honest validators are exactly enough to
//! keep the chain growing while ignoring the attacker.

use std::time::Duration;
use zksync_os_consensus_sim::{Behavior, SimCluster, links, run_scenario};

fn behaviors_with_byzantine(byzantine: Behavior) -> Vec<Behavior> {
    // Validator 0 is the attacker; the rest are honest.
    vec![
        byzantine,
        Behavior::Honest,
        Behavior::Honest,
        Behavior::Honest,
        Behavior::Honest,
    ]
}

fn byzantine_scenario(name: &'static str, byzantine: Behavior) {
    run_scenario(name, 0..3, Duration::from_secs(600), move |context| {
        let behaviors = behaviors_with_byzantine(byzantine);
        async move {
            let mut cluster =
                SimCluster::start_with_behaviors(context, &behaviors, links::healthy()).await;

            // The chain keeps growing despite the attacker (only honest validators
            // commit — the attacker runs no execution environment).
            cluster.wait_for_committed_height_all(15).await;
            cluster.assert_committed_chains_agree(15);

            // Every honest validator saw the attack for what it is, and the evidence
            // incriminates exactly the attacker.
            cluster.assert_faults_point_exactly_at(&[0]);

            // The network layer cut the attacker off; no honest validator was blocked.
            cluster.assert_blocked_only(0).await;
        }
    });
}

#[test]
fn conflicting_votes_are_detected_and_tolerated() {
    // The attacker signs two different notarize (and finalize) votes per view —
    // the canonical double-sign / equivocation attack.
    byzantine_scenario("conflicter", Behavior::Conflicter);
}

#[test]
fn nullify_and_finalize_votes_are_detected_and_tolerated() {
    // The attacker votes both to skip the view and to finalize its block — trying to
    // have it both ways.
    byzantine_scenario("nuller", Behavior::Nuller);
}
