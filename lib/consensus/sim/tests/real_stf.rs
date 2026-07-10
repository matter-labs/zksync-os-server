//! Consensus over real execution: blocks carry actual signed transfers, every validator
//! re-executes them through the production VM before voting, and the committed *state*
//! (not just the block sequence) must be identical everywhere.
//!
//! These scenarios use fewer blocks than the mock ones — each block is a real VM run —
//! but they assert strictly more: balances and nonces read from each validator's
//! committed state through the production state-view traits.

use alloy::primitives::U256;
use commonware_consensus::types::Height;
use commonware_cryptography::sha256::Digest;
use std::time::Duration;
use zksync_os_consensus_core::{BuildContext, ExecutionEnv};
use zksync_os_consensus_sim::stf::{
    RealStfExecution, StfBlock, TEST_RECIPIENT, test_sender_address,
};
use zksync_os_consensus_sim::{Behavior, SimCluster, SimEnv, links, run_scenario};

const NUM_VALIDATORS: u32 = 5;

fn honest_behaviors() -> Vec<Behavior> {
    vec![Behavior::Honest; NUM_VALIDATORS as usize]
}

/// After committing height H, the recipient holds 1 + 2 + ... + H wei (each block's
/// transfer amount encodes its height).
fn expected_recipient_balance(height: u64) -> U256 {
    U256::from(height * (height + 1) / 2)
}

/// Asserts that every validator's committed state agrees, and matches the committed
/// chain height: recipient balance, sender nonce.
fn assert_state_agreement(cluster: &SimCluster<RealStfExecution>, minimum_height: u64) {
    for &index in &cluster.honest_indices() {
        let env = &cluster.validators[index].env;
        let height = env
            .committed_nonce(test_sender_address())
            .expect("sender exists");
        assert!(
            height >= minimum_height,
            "validator {index} sender nonce {height} below expected {minimum_height}"
        );
        // The committed chain height equals the sender nonce (one transfer per block),
        // and the recipient's balance is fully determined by it.
        assert_eq!(
            env.committed_balance(TEST_RECIPIENT),
            expected_recipient_balance(height),
            "validator {index} recipient balance does not match its chain height"
        );
    }
    // All validators committed identical chains — heights may trail, prefixes must match.
    cluster.assert_committed_chains_agree(minimum_height);
}

#[test]
fn real_blocks_execute_and_commit_identically() {
    run_scenario(
        "real_stf_steady_state",
        0..2,
        Duration::from_secs(300),
        |context| async move {
            let mut cluster = SimCluster::start_with_env(
                context,
                &honest_behaviors(),
                links::healthy(),
                |_index, _context| RealStfExecution::new(),
            )
            .await;
            cluster.wait_for_committed_height_all(8).await;
            assert_state_agreement(&cluster, 8);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

#[test]
fn real_stf_validator_rejoins_after_crash() {
    run_scenario(
        "real_stf_crash_restart",
        0..2,
        Duration::from_secs(600),
        |context| async move {
            let mut cluster = SimCluster::start_with_env(
                context,
                &honest_behaviors(),
                links::healthy(),
                |_index, _context| RealStfExecution::new(),
            )
            .await;
            cluster.wait_for_committed_height_all(4).await;

            // Crash one validator. Its pending state layers die with it; only the
            // committed chain (its "disk") survives.
            cluster.crash(0);
            let survivors: Vec<usize> = (1..NUM_VALIDATORS as usize).collect();
            cluster.wait_for_committed_height(&survivors, 10).await;

            // On restart it catches up via backfill: the commit path re-executes every
            // block it missed against its committed state (there are no pending layers
            // for backfilled blocks) — the state must still converge exactly.
            cluster.restart(0).await;
            cluster.wait_for_committed_height_all(14).await;
            assert_state_agreement(&cluster, 14);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

/// A validator whose consensus stack is perfectly honest but whose execution lies:
/// every block it *builds* declares a corrupted execution-outcome hash. Verification
/// and commits stay truthful — the attack is purely "my blocks misdeclare their result".
///
/// This is the attack that verify-before-vote exists for: the lie is invisible at the
/// vote layer (the liar never equivocates, so there is no fault evidence), and the only
/// thing standing between the lie and the chain is honest validators re-executing the
/// block before voting.
#[derive(Clone)]
struct LyingStfExecution {
    inner: RealStfExecution,
    lies: bool,
}

impl ExecutionEnv for LyingStfExecution {
    type Block = StfBlock;

    async fn genesis_block(&mut self) -> StfBlock {
        self.inner.genesis_block().await
    }

    async fn build(&mut self, parent: StfBlock, context: BuildContext) -> Option<StfBlock> {
        let block = self.inner.build(parent, context).await?;
        if !self.lies {
            return Some(block);
        }
        // Re-assemble the block with a corrupted outcome commitment. Everything else —
        // parent linkage, height, transactions, signatures — stays perfectly valid, so
        // only re-execution can expose the lie.
        let mut corrupted_hash = block.block_output_hash();
        corrupted_hash.0[0] ^= 0xff;
        Some(StfBlock::assemble(
            block.height_u64(),
            block.era_anchor(),
            commonware_consensus::Block::parent(&block),
            block.timestamp(),
            block.txs().to_vec(),
            block.header_hash(),
            corrupted_hash,
        ))
    }

    async fn verify(&mut self, parent: StfBlock, block: StfBlock) -> bool {
        self.inner.verify(parent, block).await
    }

    async fn committed_height(&mut self) -> Option<Height> {
        self.inner.committed_height().await
    }

    async fn commit(&mut self, block: StfBlock) {
        self.inner.commit(block).await
    }
}

impl SimEnv for LyingStfExecution {
    fn committed_tip(&self) -> Option<u64> {
        self.inner.committed_tip()
    }

    fn committed_chain_digests(&self) -> Vec<Digest> {
        self.inner.committed_chain_digests()
    }
}

#[test]
fn lying_proposer_is_rejected_by_reexecution_without_fault_evidence() {
    run_scenario(
        "real_stf_lying_proposer",
        0..2,
        Duration::from_secs(600),
        |context| async move {
            let mut cluster = SimCluster::start_with_env(
                context,
                &honest_behaviors(),
                links::healthy(),
                // Validator 0 lies about every block it builds; the rest are truthful.
                // Note its consensus *behavior* is honest — the attack lives entirely
                // in the block content.
                |index, _context| LyingStfExecution {
                    inner: RealStfExecution::new(),
                    lies: index == 0,
                },
            )
            .await;

            // The chain keeps growing: every view led by the liar produces a block
            // that all honest verifiers re-execute, mismatch, and refuse to vote for —
            // the view times out and the next (truthful) leader re-proposes the height.
            cluster.wait_for_committed_height_all(8).await;

            // Nothing the liar built ever committed: committed state on every
            // validator (including the liar, whose commits are truthful) matches the
            // canonical per-height transfer schedule exactly.
            for &index in &cluster.honest_indices() {
                let env = &cluster.validators[index].env;
                let height = env
                    .inner
                    .committed_nonce(test_sender_address())
                    .expect("sender exists");
                assert!(height >= 8);
                assert_eq!(
                    env.inner.committed_balance(TEST_RECIPIENT),
                    expected_recipient_balance(height),
                    "validator {index} committed state diverged",
                );
            }
            cluster.assert_committed_chains_agree(8);

            // The defining property of a content-level attack: it produces NO fault
            // evidence. The liar's votes are internally consistent; only re-execution
            // caught it. (Operationally: "no fault evidence" never means "no attack".)
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}

#[test]
fn real_stf_partition_heals_with_identical_state() {
    run_scenario(
        "real_stf_partition_heal",
        0..2,
        Duration::from_secs(600),
        |context| async move {
            let mut cluster = SimCluster::start_with_env(
                context,
                &honest_behaviors(),
                links::healthy(),
                |_index, _context| RealStfExecution::new(),
            )
            .await;
            cluster.wait_for_committed_height_all(4).await;

            // Cut one validator off mid-flight. Proposals in the air around the cut
            // get abandoned — their speculative state layers must be discarded, never
            // adopted. The isolated validator stalls; the quorum side keeps going.
            // Short windows on purpose: the quorum side keeps executing real
            // blocks through them, and the isolated validator re-executes all
            // of it after the heal.
            cluster.partition(&[&[0], &[1, 2, 3, 4]]).await;
            cluster.wait_for_committed_height(&[1, 2, 3, 4], 9).await;
            cluster.settle(Duration::from_secs(3)).await;
            cluster
                .assert_no_progress_for(&[0], Duration::from_secs(8))
                .await;

            // Heal: the isolated validator backfills, re-executes what it missed, and
            // every validator's *state* — not just block sequence — must be identical.
            // Any leaked speculative layer would surface here as a balance divergence.
            cluster.heal(links::healthy()).await;
            cluster.wait_for_committed_height_all(13).await;
            assert_state_agreement(&cluster, 13);
            cluster.assert_no_faults();
            cluster.assert_no_blocked_peers().await;
        },
    );
}
