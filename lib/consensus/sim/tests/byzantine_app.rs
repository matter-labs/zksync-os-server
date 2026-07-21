//! Byzantine proposals at the application level: a committee member whose consensus
//! stack is fully honest — it votes, verifies, and commits truthfully — but whose
//! *proposals* are broken. This is a different attacker than the wire-level byzantine
//! suite (`byzantine.rs`): nothing here double-signs or equivocates, so there is no
//! attributable fault evidence and nobody gets banned. The protocol's defense is
//! plainer: honest validators refuse to vote for a broken proposal, the view times
//! out, and the next leader proposes instead — a bad proposer costs the chain exactly
//! its own leader turns.
//!
//! The *semantic* variant of this attack — a structurally perfect block whose declared
//! execution outcome is a lie — lives in `real_stf.rs`
//! (`lying_proposer_is_rejected_by_reexecution_without_fault_evidence`), where blocks
//! carry real transactions and verification re-executes them. The scenarios here pin
//! the structural variants, which mock execution expresses cheaply.

use commonware_consensus::Heightable;
use commonware_consensus::types::Height;
use commonware_cryptography::sha256::Digest;
use commonware_cryptography::{Digestible, Hasher, Sha256};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zksync_os_consensus_core::{BuildContext, ExecutionEnv};
use zksync_os_consensus_sim::{
    Behavior, MockExecution, SimBlock, SimCluster, SimEnv, links, run_scenario,
};

const NUM_VALIDATORS: usize = 5;

/// The structural ways this suite breaks a proposal.
#[derive(Clone, Copy, Debug)]
enum Sabotage {
    /// The proposal names a parent digest that exists nowhere.
    InventedParent,
    /// The proposal's height skips ahead of its parent's.
    SkippedHeight,
    /// No proposal at all — build "fails" whenever this validator leads.
    NothingAtAll,
}

/// Wraps one validator's mock execution: its proposals get sabotaged, everything else
/// — verifying other leaders' blocks, committing finalized ones — stays honest.
#[derive(Clone)]
struct SabotagedProposer {
    inner: MockExecution,
    /// `None` on honest validators (the wrapper is then a pure pass-through).
    sabotage: Option<Sabotage>,
    /// Digest of every sabotaged proposal this validator put on the wire; the
    /// scenarios assert none of them is ever committed by anyone.
    emitted: Arc<Mutex<Vec<Digest>>>,
}

impl SabotagedProposer {
    fn new(sabotage: Option<Sabotage>) -> Self {
        Self {
            inner: MockExecution::new(),
            sabotage,
            emitted: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn emitted(&self) -> Vec<Digest> {
        self.emitted.lock().unwrap().clone()
    }
}

impl ExecutionEnv for SabotagedProposer {
    type Block = SimBlock;

    async fn genesis_block(&mut self) -> SimBlock {
        self.inner.genesis_block().await
    }

    async fn build(&mut self, parent: SimBlock, context: BuildContext) -> Option<SimBlock> {
        let honest = self.inner.build(parent.clone(), context).await?;
        let Some(sabotage) = self.sabotage else {
            return Some(honest);
        };
        let broken = match sabotage {
            Sabotage::InventedParent => SimBlock::mislinked(
                honest.height().get(),
                Sha256::hash(b"a parent nobody has"),
                honest.seed(),
            ),
            Sabotage::SkippedHeight => {
                SimBlock::mislinked(honest.height().get() + 1, parent.digest(), honest.seed())
            }
            Sabotage::NothingAtAll => return None,
        };
        self.emitted.lock().unwrap().push(broken.digest());
        Some(broken)
    }

    async fn verify(&mut self, parent: SimBlock, block: SimBlock) -> bool {
        self.inner.verify(parent, block).await
    }

    async fn has_state(&mut self, block: &SimBlock) -> bool {
        self.inner.has_state(block).await
    }

    async fn committed_height(&mut self) -> Option<Height> {
        self.inner.committed_height().await
    }

    async fn adopt_committed_block(&mut self, block: &SimBlock) {
        self.inner.adopt_committed_block(block).await
    }

    async fn commit(&mut self, block: SimBlock) {
        self.inner.commit(block).await
    }
}

impl SimEnv for SabotagedProposer {
    fn committed_tip(&self) -> Option<u64> {
        self.inner.committed_tip()
    }

    fn committed_chain_digests(&self) -> Vec<Digest> {
        self.inner.committed_chain_digests()
    }
}

fn sabotage_scenario(name: &'static str, sabotage: Sabotage) {
    run_scenario(name, 0..3, Duration::from_secs(600), move |context| {
        async move {
            // Validator 0 sabotages every proposal it makes; the other four are honest.
            let saboteur = SabotagedProposer::new(Some(sabotage));
            let probe = saboteur.clone();
            let behaviors = vec![Behavior::Honest; NUM_VALIDATORS];
            let mut cluster = SimCluster::start_with_env(
                context,
                &behaviors,
                links::healthy(),
                move |index, _context| {
                    if index == 0 {
                        saboteur.clone()
                    } else {
                        SabotagedProposer::new(None)
                    }
                },
            )
            .await;

            // Liveness: the chain grows anyway. Every view led by the saboteur ends in
            // a timeout (there is nothing valid to vote for), and the next leader
            // proposes the same height. With round-robin leadership at five
            // validators, reaching height 15 rides through several sabotaged turns.
            cluster.wait_for_committed_height_all(15).await;
            cluster.assert_committed_chains_agree(15);

            // Safety: nothing the saboteur emitted was ever committed, anywhere.
            let emitted = probe.emitted();
            if !matches!(sabotage, Sabotage::NothingAtAll) {
                assert!(
                    !emitted.is_empty(),
                    "the saboteur never led a view — the scenario proved nothing"
                );
            }
            for &index in &cluster.honest_indices() {
                let chain = cluster.validators[index].env.committed_chain_digests();
                for digest in &emitted {
                    assert!(
                        !chain.contains(digest),
                        "validator {index} committed a sabotaged block"
                    );
                }
            }

            // Broken proposals are not attributable byzantine behavior: the saboteur
            // never signs two conflicting votes, so there is no fault evidence against
            // anyone, and nobody gets banned by the network layer. (Operationally:
            // "no fault evidence" never means "no attack" — the visible symptom of
            // this attacker is elevated view timeouts on its leader turns.)
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        }
    });
}

#[test]
fn invented_parent_proposals_never_finalize() {
    // The proposal links to a chain that does not exist. Honest validators cannot
    // even resolve its ancestry, let alone vote for it.
    sabotage_scenario("byzantine_app_invented_parent", Sabotage::InventedParent);
}

#[test]
fn skipped_height_proposals_never_finalize() {
    // The proposal names the right parent but skips a height. Height contiguity is
    // structural: consensus rejects the linkage before content verification runs.
    sabotage_scenario("byzantine_app_skipped_height", Sabotage::SkippedHeight);
}

#[test]
fn a_leader_that_builds_nothing_only_forfeits_its_turn() {
    // Build failures are a routine event, not an attack — but the failure mode is the
    // same as a byzantine proposer's: the view times out and the chain moves on. This
    // pins that a chronically build-less validator costs its turns and nothing else.
    sabotage_scenario("byzantine_app_builds_nothing", Sabotage::NothingAtAll);
}
