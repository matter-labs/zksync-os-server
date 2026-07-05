//! The node execution environment against the real VM: verify-by-re-execution over
//! speculative branches, and durability-paced commit into an applier.
//!
//! The state backend and the applier are small in-memory stand-ins with the same
//! contract as the node's (a strictly-linear history of per-block diffs, written only
//! when a finalized block is applied); everything else — the VM, the genesis state,
//! the replay semantics — is the production code path.

use alloy::primitives::{B256, U256};
use commonware_codec::{Encode, ReadExt};
use commonware_cryptography::Digestible;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use zksync_os_consensus_core::ExecutionEnv;
use zksync_os_consensus_execution::{
    ChainAnchor, CommittedPayload, ConsensusBlock, NodeExecutionEnv,
};
use zksync_os_consensus_sim::stf::{
    SharedGenesis, make_test_transfer, shared_genesis, test_sender_address,
};
use zksync_os_interface::tracing::{NopTracer, NopValidator};
use zksync_os_interface::traits::{NoopTxCallback, PreimageSource, ReadStorage, TxListSource};
use zksync_os_storage_api::{
    BlockContext, ReadStateHistory, ReplayRecord, StateError, StateResult, ViewState,
};
use zksync_os_types::{ZkTransaction, ZksyncOsEncode, hash_block_output};

const FUNDING: u128 = 10u128.pow(24);

/// In-memory stand-in for the node's state backend: genesis plus one diff per applied
/// block, strictly linear — the same contract the durable backend has.
#[derive(Clone)]
struct TestStateHistory {
    inner: Arc<Mutex<TestStateInner>>,
}

struct TestStateInner {
    genesis: Arc<SharedGenesis>,
    diffs: Vec<Diff>,
}

impl std::fmt::Debug for TestStateHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestStateHistory")
    }
}

impl TestStateHistory {
    fn new(genesis: Arc<SharedGenesis>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TestStateInner {
                genesis,
                diffs: Vec::new(),
            })),
        }
    }

    fn apply(&self, payload: &CommittedPayload) {
        let mut inner = self.inner.lock().unwrap();
        let output: &zksync_os_types::BlockOutput = payload.output.as_ref();
        assert_eq!(
            payload.record.block_context.block_number as usize,
            inner.diffs.len() + 1,
            "applier must receive blocks in order"
        );
        inner.diffs.push((
            output
                .storage_writes
                .iter()
                .map(|write| (write.key, write.value))
                .collect(),
            output
                .published_preimages
                .iter()
                .cloned()
                .chain(payload.record.force_preimages.iter().cloned())
                .collect(),
        ));
    }
}

/// A flattened snapshot view at a block height (test-sized; clones the maps).
#[derive(Clone)]
struct SnapshotView {
    storage: Arc<HashMap<B256, B256>>,
    preimages: Arc<HashMap<B256, Vec<u8>>>,
}

impl ReadStorage for SnapshotView {
    fn read(&mut self, key: B256) -> Option<B256> {
        self.storage.get(&key).copied()
    }
}

impl PreimageSource for SnapshotView {
    fn get_preimage(&mut self, hash: B256) -> Option<Vec<u8>> {
        self.preimages.get(&hash).cloned()
    }
}

impl TestStateHistory {
    fn snapshot_at(&self, block_number: u64) -> StateResult<SnapshotView> {
        let inner = self.inner.lock().unwrap();
        if block_number as usize > inner.diffs.len() {
            return Err(StateError::NotFound(block_number));
        }
        let mut storage = inner.genesis.storage.clone();
        let mut preimages = inner.genesis.preimages.clone();
        for (diff_storage, diff_preimages) in &inner.diffs[..block_number as usize] {
            storage.extend(diff_storage.iter().map(|(k, v)| (*k, *v)));
            preimages.extend(diff_preimages.iter().map(|(k, v)| (*k, v.clone())));
        }
        Ok(SnapshotView {
            storage: Arc::new(storage),
            preimages: Arc::new(preimages),
        })
    }
}

impl ReadStateHistory for TestStateHistory {
    fn state_view_at(&self, block_number: u64) -> StateResult<impl ViewState> {
        self.snapshot_at(block_number)
    }

    fn block_range_available(&self) -> std::ops::RangeInclusive<u64> {
        0..=(self.inner.lock().unwrap().diffs.len() as u64)
    }
}

/// The whole test rig: environment + backend + a fake applier task that persists
/// committed payloads into the backend and reports durability, like the node's
/// applier pipeline does.
struct Rig {
    env: NodeExecutionEnv<TestStateHistory>,
    base: TestStateHistory,
    genesis: Arc<SharedGenesis>,
    genesis_block: ConsensusBlock,
    /// Gate on the fake applier: `false` parks payloads unapplied — a stalled
    /// persistence pipeline. Open (`true`) by default.
    applier_gate: watch::Sender<bool>,
}

impl Rig {
    async fn new() -> Self {
        let genesis = shared_genesis(&[(test_sender_address(), U256::from(FUNDING))]);
        let base = TestStateHistory::new(genesis.clone());
        let (sink, mut payloads) = mpsc::channel::<CommittedPayload>(8);
        let (applied_sender, applied) = watch::channel(None);
        let (applier_gate, mut gate) = watch::channel(true);

        // Fake applier: fold each payload into the backend, then report it durable.
        // Holds payloads while the gate is closed, like a stalled pipeline would.
        let applier_base = base.clone();
        tokio::spawn(async move {
            while let Some(payload) = payloads.recv().await {
                if gate.wait_for(|open| *open).await.is_err() {
                    return;
                }
                let number = payload.record.block_context.block_number;
                applier_base.apply(&payload);
                let _ = applied_sender.send(Some(number));
            }
        });

        let anchor = ChainAnchor {
            genesis_block_hash: genesis.header_hash,
            genesis_timestamp: genesis.context.timestamp,
            genesis_protocol_version: "0.31.0".parse().expect("valid version"),
            genesis_fee_params: zksync_os_sequencer::execution::FeeParams {
                eip1559_basefee: genesis.context.eip1559_basefee,
                native_price: genesis.context.native_price,
                pubdata_price: genesis.context.pubdata_price,
            },
        };
        let mut env = NodeExecutionEnv::new(base.clone(), anchor, 0, None, sink, applied, 0);
        let genesis_block = env.genesis_block().await;
        Self {
            env,
            base,
            genesis,
            genesis_block,
            applier_gate,
        }
    }

    /// Produces a valid replayable block on top of `parent` by executing `txs` through
    /// the production VM — the honest-proposer stand-in until block building lands.
    /// `parent_el_hash` is the execution-layer hash of the parent block (the genesis
    /// header hash for block 1). `parent_diff` supplies the parent's own state changes
    /// when the parent is itself an uncommitted candidate (the backend only has
    /// committed state). Returns the record, the block's execution-layer hash, and its
    /// state diff for producing further children.
    async fn produce_record(
        &self,
        parent: Option<&ReplayRecord>,
        parent_el_hash: B256,
        parent_diff: Option<&Diff>,
        timestamp: u64,
        txs: Vec<ZkTransaction>,
    ) -> (ReplayRecord, B256, Diff) {
        let genesis_context = &self.genesis.context;
        let (number, ring, previous_timestamp) = match parent {
            Some(parent) => (
                parent.block_context.block_number + 1,
                parent.block_context.block_hashes.push(parent_el_hash),
                parent.block_context.timestamp,
            ),
            None => (
                1,
                genesis_context.block_hashes.push(parent_el_hash),
                genesis_context.timestamp,
            ),
        };
        let context = BlockContext {
            chain_id: genesis_context.chain_id,
            block_number: number,
            block_hashes: ring,
            timestamp,
            eip1559_basefee: U256::from(1_000_000_000u64),
            pubdata_price: U256::ZERO,
            native_price: U256::from(1u64),
            coinbase: alloy::primitives::Address::ZERO,
            gas_limit: 100_000_000,
            pubdata_limit: 100_000_000,
            mix_hash: U256::ZERO,
            execution_version: genesis_context.execution_version,
            blob_fee: U256::ONE,
        };
        let committed = *self.base.block_range_available().end();
        let mut view = self
            .base
            .snapshot_at(committed.min(number - 1))
            .expect("committed state must exist for production");
        if let Some((storage, preimages)) = parent_diff {
            let mut merged_storage = (*view.storage).clone();
            let mut merged_preimages = (*view.preimages).clone();
            merged_storage.extend(storage.iter().map(|(k, v)| (*k, *v)));
            merged_preimages.extend(preimages.iter().map(|(k, v)| (*k, v.clone())));
            view = SnapshotView {
                storage: Arc::new(merged_storage),
                preimages: Arc::new(merged_preimages),
            };
        }
        let output = zksync_os_multivm::run_block(
            context,
            view.clone(),
            view,
            TxListSource {
                transactions: txs.iter().map(|tx| tx.clone().encode()).collect(),
            },
            NoopTxCallback,
            &mut NopTracer,
            &mut NopValidator,
        )
        .expect("production execution failed");
        let el_hash = output.header.hash();
        let diff: Diff = (
            output
                .storage_writes
                .iter()
                .map(|write| (write.key, write.value))
                .collect(),
            output.published_preimages.iter().cloned().collect(),
        );
        let record = ReplayRecord::new(
            context,
            txs,
            previous_timestamp,
            semver::Version::new(0, 0, 0),
            "0.31.0".parse().expect("valid version"),
            hash_block_output(&output),
            Vec::new(),
            Default::default(),
        );
        (record, el_hash, diff)
    }
}

/// A block's state changes: storage writes and published preimages.
type Diff = (HashMap<B256, B256>, HashMap<B256, Vec<u8>>);

fn transfer(genesis: &SharedGenesis, nonce: u64) -> ZkTransaction {
    make_test_transfer(genesis.context.chain_id, nonce, U256::from(1u64)).into()
}

#[tokio::test]
async fn verify_accepts_honest_block_and_commit_persists_it() {
    let mut rig = Rig::new().await;
    let (record, _el, _diff) = rig
        .produce_record(
            None,
            rig.genesis.header_hash,
            None,
            rig.genesis.context.timestamp + 1,
            vec![transfer(&rig.genesis, 0)],
        )
        .await;
    let block = ConsensusBlock::from_record(&rig.genesis_block, record);

    assert!(
        rig.env
            .verify(rig.genesis_block.clone(), block.clone())
            .await
    );

    rig.env.commit(block).await;
    assert_eq!(
        rig.env.committed_height().await.map(|h| h.get()),
        Some(1),
        "commit must advance the durable head"
    );
    // The backend now durably has block 1 — the sender's nonce moved.
    let mut view = rig.base.state_view_at(1).expect("state at 1");
    assert_eq!(view.nonce(test_sender_address()), Some(1));
}

#[tokio::test]
async fn verify_rejects_a_misdeclared_execution_outcome() {
    let mut rig = Rig::new().await;
    let (mut record, _el, _diff) = rig
        .produce_record(
            None,
            rig.genesis.header_hash,
            None,
            rig.genesis.context.timestamp + 1,
            vec![transfer(&rig.genesis, 0)],
        )
        .await;
    // The proposer lies about what executing the block does.
    record.block_output_hash.0[0] ^= 0xff;
    let block = ConsensusBlock::from_record(&rig.genesis_block, record);

    assert!(
        !rig.env.verify(rig.genesis_block.clone(), block).await,
        "a block whose declared outcome does not reproduce must be rejected"
    );
}

#[tokio::test]
async fn competing_branches_are_isolated_and_the_loser_is_pruned() {
    let mut rig = Rig::new().await;
    // Two competing proposals for height 1 (different timestamps → different blocks).
    let (record_a, el_a, diff_a) = rig
        .produce_record(
            None,
            rig.genesis.header_hash,
            None,
            rig.genesis.context.timestamp + 1,
            vec![transfer(&rig.genesis, 0)],
        )
        .await;
    let (record_b, _el_b, _diff_b) = rig
        .produce_record(
            None,
            rig.genesis.header_hash,
            None,
            rig.genesis.context.timestamp + 2,
            vec![transfer(&rig.genesis, 0)],
        )
        .await;
    let block_a = ConsensusBlock::from_record(&rig.genesis_block, record_a.clone());
    let block_b = ConsensusBlock::from_record(&rig.genesis_block, record_b);
    assert_ne!(block_a.digest(), block_b.digest());

    assert!(
        rig.env
            .verify(rig.genesis_block.clone(), block_a.clone())
            .await
    );
    assert!(
        rig.env
            .verify(rig.genesis_block.clone(), block_b.clone())
            .await
    );

    // A child of candidate A verifies against A's speculative state (the sender's
    // nonce is already 1 on that branch).
    let (record_a2, _, _) = rig
        .produce_record(
            Some(&record_a),
            el_a,
            Some(&diff_a),
            rig.genesis.context.timestamp + 3,
            vec![transfer(&rig.genesis, 1)],
        )
        .await;
    let block_a2 = ConsensusBlock::from_record(&block_a, record_a2);
    assert!(rig.env.verify(block_a.clone(), block_a2.clone()).await);

    // Candidate A wins height 1.
    rig.env.commit(block_a.clone()).await;

    // The loser can no longer be anyone's parent; the winner's child still can.
    let (record_b2, _, _) = rig
        .produce_record(
            Some(&record_a),
            el_a,
            Some(&diff_a),
            rig.genesis.context.timestamp + 4,
            vec![transfer(&rig.genesis, 1)],
        )
        .await;
    let fake_child_of_b = ConsensusBlock::from_record(&block_b, record_b2);
    assert!(
        !rig.env.verify(block_b, fake_child_of_b).await,
        "children of an abandoned candidate must not verify"
    );
    rig.env.commit(block_a2).await;
    assert_eq!(rig.env.committed_height().await.map(|h| h.get()), Some(2));
}

#[tokio::test]
async fn commit_reexecutes_when_the_block_was_never_verified_locally() {
    let mut rig = Rig::new().await;
    let (record, _el, _diff) = rig
        .produce_record(
            None,
            rig.genesis.header_hash,
            None,
            rig.genesis.context.timestamp + 1,
            vec![transfer(&rig.genesis, 0)],
        )
        .await;
    let block = ConsensusBlock::from_record(&rig.genesis_block, record);

    // No verify happened (this is the backfill/restart path): commit must re-execute
    // the block against the durable chain and still converge.
    rig.env.commit(block.clone()).await;
    assert_eq!(rig.env.committed_height().await.map(|h| h.get()), Some(1));

    // Re-delivery of the committed tip is a no-op (at-least-once delivery).
    rig.env.commit(block).await;
    assert_eq!(rig.env.committed_height().await.map(|h| h.get()), Some(1));
}

#[test]
fn consensus_block_codec_roundtrips() {
    let genesis = ConsensusBlock::genesis(B256::repeat_byte(7));
    let encoded = genesis.encode().to_vec();
    let decoded = ConsensusBlock::read(&mut encoded.as_slice()).expect("decode genesis");
    assert_eq!(decoded.digest(), genesis.digest());
    assert_eq!(decoded.height_u64(), 0);
    assert!(decoded.record().is_none());
}

#[tokio::test]
async fn digest_is_independent_of_node_version() {
    // The same logical block built by different node releases must have the same
    // consensus identity — `node_version` is binary metadata, not block content. The
    // wire encoding omits it by design; this pins that property against codec changes.
    let rig = Rig::new().await;
    let (record, _el, _diff) = rig
        .produce_record(
            None,
            rig.genesis.header_hash,
            None,
            rig.genesis.context.timestamp + 1,
            vec![transfer(&rig.genesis, 0)],
        )
        .await;
    let mut relabeled = record.clone();
    relabeled.node_version = semver::Version::new(9, 9, 9);

    let block = ConsensusBlock::from_record(&rig.genesis_block, record);
    let relabeled_block = ConsensusBlock::from_record(&rig.genesis_block, relabeled);
    assert_eq!(block.digest(), relabeled_block.digest());
}

/// A validator with no local L1 knowledge at all — sufficient for blocks that carry
/// no L1-sourced inputs.
struct NoL1Inputs;

#[async_trait::async_trait]
impl zksync_os_consensus_execution::LocalL1Inputs for NoL1Inputs {
    async fn seen_priority_tx(
        &self,
        _id: zksync_os_types::L1TxSerialId,
    ) -> (
        Option<Arc<zksync_os_types::L1PriorityEnvelope>>,
        Option<zksync_os_types::L1TxSerialId>,
    ) {
        (None, None)
    }

    async fn seen_upgrade(
        &self,
        _version: &zksync_os_types::ProtocolSemanticVersion,
    ) -> Option<zksync_os_types::UpgradeInfo> {
        None
    }
}

/// Validity rules pinned to exactly what the rig's producer emits (fee overrides pin
/// the fee rules; the chain constants mirror `produce_record`). Tests lower
/// `max_transactions` to provoke the cap verdict.
fn rig_validation(
    rig: &Rig,
    max_transactions: usize,
) -> zksync_os_consensus_execution::ProposalValidation {
    use num::rational::Ratio;
    use zksync_os_consensus_execution::{ProposalValidation, ValidityConfig};

    ProposalValidation {
        config: Arc::new(ValidityConfig {
            max_timestamp_skew: std::time::Duration::from_secs(3600),
            chain_id: rig.genesis.context.chain_id,
            fee_collector_address: alloy::primitives::Address::ZERO,
            gas_limit: 100_000_000,
            pubdata_limit: 100_000_000,
            max_transactions,
            max_encoded_record_size: 16 * 1024 * 1024,
            fee: zksync_os_sequencer::execution::FeeConfig {
                native_price_usd: Ratio::from_integer(1u32.into()),
                base_fee_override: Some(1_000_000_000u64.into()),
                native_per_gas: 1,
                pubdata_price_override: Some(0u64.into()),
                pubdata_price_cap: None,
                native_price_override: Some(1u64.into()),
            },
        }),
        inputs: Arc::new(NoL1Inputs),
    }
}

#[tokio::test]
async fn validity_rules_run_before_re_execution() {
    let mut rig = Rig::new().await;
    rig.env = rig.env.clone().with_validity(rig_validation(&rig, 100));

    // An honest block sails through the rules and re-execution alike.
    let (record, _el, _diff) = rig
        .produce_record(
            None,
            rig.genesis.header_hash,
            None,
            rig.genesis.context.timestamp + 1,
            vec![transfer(&rig.genesis, 0)],
        )
        .await;
    let block = ConsensusBlock::from_record(&rig.genesis_block, record.clone());
    assert!(rig.env.verify(rig.genesis_block.clone(), block).await);

    // A far-future timestamp fails the rules (well before re-execution could object —
    // the VM executes any timestamp just fine).
    let far_future = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs())
        + 2 * 3600;
    let (mut record, _el, _diff) = rig
        .produce_record(
            None,
            rig.genesis.header_hash,
            None,
            far_future,
            vec![transfer(&rig.genesis, 0)],
        )
        .await;
    record.block_context.timestamp = far_future;
    let block = ConsensusBlock::from_record(&rig.genesis_block, record);
    assert!(
        !rig.env.verify(rig.genesis_block.clone(), block).await,
        "a timestamp beyond the allowed skew must be rejected"
    );
}

/// The block-size caps run inside the same rule pass: with the transaction cap set to
/// zero, an otherwise perfectly honest one-transfer block is rejected before any
/// re-execution happens.
#[tokio::test]
async fn transaction_cap_is_enforced_at_verification() {
    let mut rig = Rig::new().await;
    rig.env = rig.env.clone().with_validity(rig_validation(&rig, 0));

    let (record, _el, _diff) = rig
        .produce_record(
            None,
            rig.genesis.header_hash,
            None,
            rig.genesis.context.timestamp + 1,
            vec![transfer(&rig.genesis, 0)],
        )
        .await;
    let block = ConsensusBlock::from_record(&rig.genesis_block, record);
    assert!(
        !rig.env.verify(rig.genesis_block.clone(), block).await,
        "a block over the transaction cap must be rejected"
    );
}

/// A stalled persistence pipeline must not stop this validator from verifying (and
/// voting) — but the speculative state it accumulates meanwhile must stay bounded,
/// and the backlog must drain cleanly once the pipeline recovers.
#[tokio::test]
async fn a_stalled_pipeline_bounds_speculation_and_drains_cleanly() {
    let mut rig = Rig::new().await;
    rig.env = rig.env.clone().with_pending_cap(3);

    // A chain of five empty blocks; diffs accumulate so each next block is produced
    // against its full ancestor state.
    let mut records = Vec::new();
    let mut parent: Option<(ReplayRecord, B256)> = None;
    let mut merged: Diff = (HashMap::new(), HashMap::new());
    for i in 1..=5u64 {
        let (parent_record, parent_el) = match &parent {
            Some((record, el)) => (Some(record), *el),
            None => (None, rig.genesis.header_hash),
        };
        let has_ancestors = !merged.0.is_empty() || !merged.1.is_empty();
        let (record, el, diff) = rig
            .produce_record(
                parent_record,
                parent_el,
                has_ancestors.then_some(&merged),
                rig.genesis.context.timestamp + i,
                Vec::new(),
            )
            .await;
        merged.0.extend(diff.0.iter().map(|(k, v)| (*k, *v)));
        merged.1.extend(diff.1.iter().map(|(k, v)| (*k, v.clone())));
        records.push(record.clone());
        parent = Some((record, el));
    }
    let mut blocks = Vec::new();
    let mut parent_block = rig.genesis_block.clone();
    for record in records {
        let block = ConsensusBlock::from_record(&parent_block, record);
        blocks.push(block.clone());
        parent_block = block;
    }

    // Stall the pipeline, then keep verifying: consensus participation must not
    // depend on the applier making progress.
    rig.applier_gate.send(false).expect("applier alive");
    let mut committing = rig.env.clone();
    let first = blocks[0].clone();
    let stalled_commit = tokio::spawn(async move { committing.commit(first).await });

    assert!(
        rig.env
            .verify(rig.genesis_block.clone(), blocks[0].clone())
            .await
    );
    assert!(rig.env.verify(blocks[0].clone(), blocks[1].clone()).await);
    assert!(rig.env.verify(blocks[1].clone(), blocks[2].clone()).await);

    // Three speculative blocks held, cap reached: new blocks are not vouched for...
    assert!(
        !rig.env.verify(blocks[2].clone(), blocks[3].clone()).await,
        "the speculative-state cap must withhold beyond capacity",
    );
    // ...but re-verifying an already-held block adds nothing and stays allowed.
    assert!(rig.env.verify(blocks[1].clone(), blocks[2].clone()).await);

    // The pipeline recovers: the parked commit drains, pruning its block from the
    // speculative set, and verification of new blocks resumes.
    rig.applier_gate.send(true).expect("applier alive");
    stalled_commit.await.expect("commit task");
    assert!(rig.env.verify(blocks[2].clone(), blocks[3].clone()).await);

    // The rest of the chain commits and the durable head converges.
    for block in &blocks[1..=3] {
        rig.env.commit(block.clone()).await;
    }
    assert_eq!(
        rig.env.committed_height().await.map(|h| h.get()),
        Some(4),
        "the backlog must drain to the durable chain",
    );
    assert!(rig.env.verify(blocks[3].clone(), blocks[4].clone()).await);
}
