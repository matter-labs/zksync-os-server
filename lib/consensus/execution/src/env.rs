//! The node's execution environment for consensus.
//!
//! This is where consensus meets the real node: blocks are verified by re-executing
//! them through the production VM against speculative state, and finalized blocks are
//! handed to the node's persistence pipeline (write-ahead log first, then state,
//! repositories, tree) with consensus paced by durability.
//!
//! Verification is two checks in sequence: the proposal validity rules (see
//! [`crate::rules`] — bounding what a leader may put in a block) and full re-execution
//! (proving the declared outcome). Building runs through the attached
//! [`BuildBlocks`] implementation when this validator leads.

use crate::builder::{BuildBlocks, BuiltBlock, ParentInfo, derive_next_cursors};
use crate::metrics::CONSENSUS_METRICS;
use crate::pending_state::{BranchOverrides, CommittedHead, Overlay, PendingState};
use crate::rules::{LocalL1Inputs, ParentView, ValidityConfig, Verdict};
use commonware_consensus::types::Height;
use commonware_cryptography::Digestible;
use commonware_cryptography::sha256::Digest;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};
use zksync_os_consensus_core::{BuildContext, ExecutionEnv};
use zksync_os_interface::tracing::{NopTracer, NopValidator};
use zksync_os_interface::traits::{PreimageSource, ReadStorage};
use zksync_os_observability::ComponentStateReporter;
use zksync_os_sequencer::execution::FeeParams;
use zksync_os_sequencer::execution::block_context_provider::millis_since_epoch;
use zksync_os_sequencer::execution::execute_block_in_vm::execute_block_in_vm;
use zksync_os_sequencer::model::blocks::{
    BlockCommandType, BlockOutputWithReads, BlockPayload, PreparedBlockCommand,
};
use zksync_os_storage_api::BlockHashes;
use zksync_os_storage_api::state_override_view::OverriddenStateView;
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::{ProtocolSemanticVersion, ZkEnvelope};
use zksync_os_wire::ConsensusBlock;

/// Chain-level constants the environment needs to anchor the chain root.
#[derive(Debug, Clone)]
pub struct ChainAnchor {
    /// Hash of the genesis block header.
    pub genesis_block_hash: alloy::primitives::B256,
    /// Timestamp of the genesis block.
    pub genesis_timestamp: u64,
    /// Protocol version the chain starts at.
    pub genesis_protocol_version: ProtocolSemanticVersion,
    /// Fee parameters of the genesis block — the base of the per-block fee clamps for
    /// the first block built on it.
    pub genesis_fee_params: FeeParams,
}

/// A self-contained read view of the node state at a fixed block height.
///
/// The backend's native views borrow from the backend handle, which would tie them to a
/// lock held across VM execution. This wrapper owns a backend handle (cheap to clone)
/// and opens a fresh view per read — view construction is O(1) on the default backend.
#[derive(Clone)]
pub struct BaseViewAt<S> {
    base: S,
    height: u64,
}

impl<S: ReadStateHistory + Clone + Send + 'static> ReadStorage for BaseViewAt<S> {
    fn read(&mut self, key: alloy::primitives::B256) -> Option<alloy::primitives::B256> {
        self.base
            .state_view_at(self.height)
            .expect("committed state view must be available")
            .read(key)
    }
}

impl<S: ReadStateHistory + Clone + Send + 'static> PreimageSource for BaseViewAt<S> {
    fn get_preimage(&mut self, hash: alloy::primitives::B256) -> Option<Vec<u8>> {
        self.base
            .state_view_at(self.height)
            .expect("committed state view must be available")
            .get_preimage(hash)
    }
}

/// The state view a build executes against: the parent branch's overlays over the
/// committed base.
pub type EnvView<S> = OverriddenStateView<BaseViewAt<S>, BranchOverrides>;

/// What the environment hands the node's persistence pipeline per finalized block.
/// Identical to what the pre-consensus pipeline's canonization fence emitted, so the
/// existing applier tail consumes it unchanged.
pub type CommittedPayload = BlockPayload;

#[derive(Clone)]
pub struct NodeExecutionEnv<S>
where
    S: ReadStateHistory + Clone + Send + Sync + 'static,
{
    base: S,
    anchor: ChainAnchor,
    genesis: ConsensusBlock,
    shared: Arc<Mutex<Shared>>,
    /// Finalized payloads flow into the node's persistence pipeline through here.
    committed_sink: mpsc::Sender<CommittedPayload>,
    /// The applier reports durably persisted block numbers here; commits are
    /// acknowledged to consensus only once the write-ahead log has the block.
    applied: watch::Receiver<Option<u64>>,
    reporter: ComponentStateReporter,
    /// Consistency input the proposer and verifiers must agree on; part of the chain
    /// configuration.
    interop_roots_per_block: u64,
    /// Builds blocks when this validator leads. `None` = never propose (verification
    /// and commits still work — the configuration for tests and passive followers).
    builder: Option<Arc<tokio::sync::Mutex<dyn BuildBlocks<EnvView<S>>>>>,
    /// Proposal validity rules and the local L1 view they authenticate against.
    /// `None` = verification is structural checks + re-execution only (a test-only
    /// configuration; production wiring always attaches this).
    validity: Option<ProposalValidation>,
    /// Upper bound on uncommitted speculative blocks held in memory. When the node's
    /// persistence pipeline stalls, consensus keeps verifying and voting (by design —
    /// a slow disk must not silence a validator), so speculative state accumulates;
    /// this bound turns unbounded growth into withheld votes once reached, and the
    /// backlog self-heals as commits drain.
    pending_cap: usize,
    /// The node's sovereign finality store. Commit records each finalized block's
    /// height→digest here (the certificate half is written by the consensus activity
    /// observer); `None` in tests that do not care about certificates.
    finality: Option<Arc<crate::finality_store::FinalityStore>>,
}

/// Default for [`NodeExecutionEnv`]'s speculative-block bound: far above any healthy
/// unfinalized window (a handful of blocks), small enough that a stalled node stops
/// accumulating long before memory pressure.
const DEFAULT_PENDING_CAP: usize = 128;

/// The inputs proposal-validity checking needs (see [`crate::rules`]).
#[derive(Clone)]
pub struct ProposalValidation {
    pub config: Arc<ValidityConfig>,
    pub inputs: Arc<dyn LocalL1Inputs>,
}

struct Shared {
    pending: PendingState,
    /// Execution outputs of pending blocks, kept for the eventual commit so finalized
    /// blocks are not re-executed on the happy path.
    outputs: HashMap<Digest, BlockOutputWithReads>,
    /// Execution-layer hash of the committed tip — the block-hash ring extension when
    /// building directly on it. `None` after a restart until the node wiring supplies
    /// it (or the first commit sets it); building waits, verification does not care.
    committed_el_hash: Option<alloy::primitives::B256>,
    /// The persistence pipeline has gone away — the node is shutting down. Commits
    /// become no-ops from that point (nothing durable can happen anymore, and after a
    /// restart consensus re-delivers from the durable height anyway).
    pipeline_closed: bool,
}

impl<S> NodeExecutionEnv<S>
where
    S: ReadStateHistory + Clone + Send + Sync + 'static,
{
    /// `committed_height` is where the durable chain currently ends (the write-ahead
    /// log tip; 0 for a fresh chain).
    pub fn new(
        base: S,
        anchor: ChainAnchor,
        committed_height: u64,
        committed_el_hash: Option<alloy::primitives::B256>,
        committed_sink: mpsc::Sender<CommittedPayload>,
        applied: watch::Receiver<Option<u64>>,
        interop_roots_per_block: u64,
    ) -> Self {
        let genesis = ConsensusBlock::genesis(anchor.genesis_block_hash);
        let committed_el_hash =
            committed_el_hash.or((committed_height == 0).then_some(anchor.genesis_block_hash));
        let committed = CommittedHead {
            height: committed_height,
            // Consensus digests are not persisted in the node's storage; after a
            // restart above genesis the tip's digest starts out unknown, and the
            // consensus stack hands it back from its block archive at startup (see
            // `adopt_committed_block`). Until then — or if the archive lost it —
            // parents at the committed height are matched by height alone (consensus
            // has already validated their ancestry).
            digest: (committed_height == 0).then(|| genesis.digest()),
        };
        Self {
            base,
            anchor,
            genesis,
            shared: Arc::new(Mutex::new(Shared {
                pending: PendingState::new(committed),
                outputs: HashMap::new(),
                committed_el_hash,
                pipeline_closed: false,
            })),
            committed_sink,
            applied,
            reporter: ComponentStateReporter::new("consensus_execution").0,
            interop_roots_per_block,
            builder: None,
            validity: None,
            pending_cap: DEFAULT_PENDING_CAP,
            finality: None,
        }
    }

    /// Overrides the speculative-block bound (tests exercise the bound with small
    /// values; production keeps the default).
    pub fn with_pending_cap(mut self, cap: usize) -> Self {
        self.pending_cap = cap;
        self
    }

    /// Attaches the node's finality store — the commit path records every finalized
    /// block's height→digest mapping in it (the certificate half arrives from the
    /// consensus activity observer).
    pub fn with_finality_store(mut self, store: Arc<crate::finality_store::FinalityStore>) -> Self {
        self.finality = Some(store);
        self
    }

    /// Attaches the block builder — required on validators that should propose.
    pub fn with_builder(
        mut self,
        builder: Arc<tokio::sync::Mutex<dyn BuildBlocks<EnvView<S>>>>,
    ) -> Self {
        self.builder = Some(builder);
        self
    }

    /// Attaches the proposal validity rules — required on every production validator
    /// (without them, verification cannot tell an honest leader's L1 inputs from
    /// fabricated ones).
    pub fn with_validity(mut self, validation: ProposalValidation) -> Self {
        self.validity = Some(validation);
        self
    }

    /// The inputs block production needs about a parent, or `None` when they are not
    /// at hand (unknown pending parent, or the committed tip's execution-layer hash is
    /// missing right after a restart).
    fn parent_info(&self, parent: &ConsensusBlock) -> Option<ParentInfo> {
        let shared = self.shared.lock().unwrap();
        let (el_hash, record) = match parent.record() {
            // The genesis block: everything comes from the chain anchor.
            None => {
                return Some(ParentInfo {
                    number: 0,
                    timestamp: self.anchor.genesis_timestamp,
                    el_hash: self.anchor.genesis_block_hash,
                    block_hashes: BlockHashes::default(),
                    protocol_version: self.anchor.genesis_protocol_version.clone(),
                    next_cursors: Default::default(),
                    carries_upgrade_tx: false,
                    fee_params: self.anchor.genesis_fee_params,
                    digest: parent.digest(),
                });
            }
            Some(record) => {
                let el_hash = if let Some(output) = shared.outputs.get(&parent.digest()) {
                    output.as_ref().header.hash()
                } else if shared.pending.committed().height == parent.height_u64() {
                    shared.committed_el_hash?
                } else {
                    return None;
                };
                (el_hash, record)
            }
        };
        Some(ParentInfo {
            number: record.block_context.block_number,
            timestamp: record.block_context.timestamp,
            el_hash,
            block_hashes: record.block_context.block_hashes,
            protocol_version: record.protocol_version.clone(),
            next_cursors: derive_next_cursors(record),
            carries_upgrade_tx: record
                .transactions
                .iter()
                .any(|tx| matches!(tx.envelope(), ZkEnvelope::Upgrade(_))),
            fee_params: FeeParams {
                eip1559_basefee: record.block_context.eip1559_basefee,
                native_price: record.block_context.native_price,
                pubdata_price: record.block_context.pubdata_price,
            },
            digest: parent.digest(),
        })
    }

    fn view_on(&self, branch: BranchOverrides, committed_height: u64) -> EnvView<S> {
        OverriddenStateView::new(
            BaseViewAt {
                base: self.base.clone(),
                height: committed_height,
            },
            branch,
        )
    }

    /// Structural consistency between a block's context and its parent — the same
    /// invariants the node's replay path enforces on blocks arriving from outside.
    /// Bounding what a leader may put *inside* a structurally-sound block (timestamps,
    /// fees, L1-input authenticity) is [`crate::rules`]'s job, in `verify` right after
    /// these checks.
    fn check_linkage(&self, parent: &ConsensusBlock, record: &ReplayRecord) -> Result<(), String> {
        let context = &record.block_context;
        match parent.record() {
            Some(parent_record) => {
                let parent_context = &parent_record.block_context;
                if parent_context.block_number + 1 != context.block_number {
                    return Err(format!(
                        "block number {} does not follow parent {}",
                        context.block_number, parent_context.block_number
                    ));
                }
                if parent_context.timestamp != record.previous_block_timestamp {
                    return Err(format!(
                        "declared previous timestamp {} does not match parent's {}",
                        record.previous_block_timestamp, parent_context.timestamp
                    ));
                }
                if parent_context.block_hashes.0[1..] != context.block_hashes.0[..255] {
                    return Err("block-hash ring does not extend the parent's".to_string());
                }
            }
            None => {
                // Parent is the genesis block.
                if context.block_number != 1 {
                    return Err(format!(
                        "block number {} does not follow genesis",
                        context.block_number
                    ));
                }
                if record.previous_block_timestamp != self.anchor.genesis_timestamp {
                    return Err(format!(
                        "declared previous timestamp {} does not match genesis timestamp {}",
                        record.previous_block_timestamp, self.anchor.genesis_timestamp
                    ));
                }
                let expected =
                    alloy::primitives::U256::from_be_bytes(self.anchor.genesis_block_hash.0);
                if context.block_hashes.0[255] != expected {
                    return Err("block-hash ring does not end with the genesis hash".to_string());
                }
            }
        }
        Ok(())
    }

    /// Executes a record replay-style (all transactions known upfront, outcome
    /// commitment enforced) against the given branch view.
    async fn execute_record(
        &self,
        branch: BranchOverrides,
        record: &ReplayRecord,
    ) -> Result<BlockOutputWithReads, String> {
        let committed_height = { self.shared.lock().unwrap().pending.committed().height };
        let view = self.view_on(branch, committed_height);

        let command = PreparedBlockCommand::for_replay(
            record.clone(),
            "consensus_verify",
            self.interop_roots_per_block,
        );

        match execute_block_in_vm(command, view, &self.reporter, NopTracer, NopValidator).await {
            Ok((output, _record, _failed, _)) => Ok(output),
            // Any failure to reproduce the declared outcome — invalid transaction,
            // execution error, output-hash mismatch — lands here.
            Err(dump) => Err(dump.error),
        }
    }

    fn overlay_of(output: &BlockOutputWithReads, record: &ReplayRecord) -> Overlay {
        Overlay::new(
            output
                .as_ref()
                .storage_writes
                .iter()
                .map(|write| (write.key, write.value)),
            output
                .as_ref()
                .published_preimages
                .iter()
                .cloned()
                .chain(record.force_preimages.iter().cloned()),
        )
    }
}

impl<S> ExecutionEnv for NodeExecutionEnv<S>
where
    S: ReadStateHistory + Clone + Send + Sync + 'static,
{
    type Block = ConsensusBlock;

    async fn genesis_block(&mut self) -> ConsensusBlock {
        self.genesis.clone()
    }

    async fn build(
        &mut self,
        parent: ConsensusBlock,
        _context: BuildContext,
    ) -> Option<ConsensusBlock> {
        // Declining to propose is always safe: consensus times the view out and moves
        // to the next leader. Every early return below is exactly that.
        let Some(builder) = self.builder.clone() else {
            warn!("no block builder attached; passing this leader turn");
            return None;
        };
        let Some(parent_info) = self.parent_info(&parent) else {
            warn!(
                parent = parent.height_u64(),
                "parent inputs unavailable; passing this leader turn"
            );
            return None;
        };
        let (branch, committed_height) = {
            let shared = self.shared.lock().unwrap();
            if shared.pending.len() >= self.pending_cap {
                warn!(
                    pending = shared.pending.len(),
                    "speculative state at capacity (commits lagging?); passing this leader turn"
                );
                return None;
            }
            let branch = shared
                .pending
                .branch_for_parent(parent.height_u64(), parent_info.digest);
            (branch, shared.pending.committed().height)
        };
        let Some(branch) = branch else {
            warn!(
                parent = parent.height_u64(),
                "parent state unavailable; passing this leader turn"
            );
            return None;
        };

        let view = self.view_on(branch, committed_height);
        let Some(BuiltBlock { record, output, .. }) =
            builder.lock().await.build_block(&parent_info, view).await
        else {
            CONSENSUS_METRICS.build_outcomes[&"passed"].inc();
            return None;
        };

        let block = ConsensusBlock::from_record(&parent, record);
        {
            let mut shared = self.shared.lock().unwrap();
            let overlay = Self::overlay_of(&output, block.record().expect("just built"));
            shared.pending.insert(
                block.digest(),
                block.height_u64(),
                parent_info.digest,
                overlay,
            );
            shared.outputs.insert(block.digest(), output);
        }
        CONSENSUS_METRICS.build_outcomes[&"built"].inc();
        Some(block)
    }

    async fn verify(&mut self, parent: ConsensusBlock, block: ConsensusBlock) -> bool {
        let Some(record) = block.record() else {
            warn!("received a payload-less block above genesis; rejecting");
            CONSENSUS_METRICS.verify_verdicts[&"invalid"].inc();
            return false;
        };
        if let Err(reason) = self.check_linkage(&parent, record) {
            warn!(
                height = block.height_u64(),
                reason, "block failed linkage checks; rejecting"
            );
            CONSENSUS_METRICS.verify_verdicts[&"invalid"].inc();
            return false;
        }
        if let Some(validation) = self.validity.clone() {
            let parent_view = match parent.record() {
                Some(parent_record) => crate::rules::parent_view_of_record(parent_record),
                None => ParentView {
                    timestamp: self.anchor.genesis_timestamp,
                    protocol_version: self.anchor.genesis_protocol_version.clone(),
                    next_cursors: Default::default(),
                    fee_params: self.anchor.genesis_fee_params,
                },
            };
            let now_epoch_seconds = (millis_since_epoch() / 1000) as u64;
            let verdict = crate::rules::check_proposal(
                &parent_view,
                record,
                block.encoded_record_len(),
                now_epoch_seconds,
                validation.inputs.as_ref(),
                &validation.config,
            )
            .await;
            match verdict {
                Verdict::Valid => {}
                Verdict::Withhold(reason) => {
                    // Not a fault: this node's L1 view lags the leader's. No vote this
                    // round; a later round re-verifies against fresher knowledge.
                    info!(
                        height = block.height_u64(),
                        reason, "cannot validate proposal inputs yet; withholding vote"
                    );
                    CONSENSUS_METRICS.verify_verdicts[&"withhold"].inc();
                    return false;
                }
                Verdict::Invalid(reason) => {
                    warn!(
                        height = block.height_u64(),
                        reason, "block failed validity rules; rejecting"
                    );
                    CONSENSUS_METRICS.verify_verdicts[&"invalid"].inc();
                    return false;
                }
            }
        }
        let branch = {
            let shared = self.shared.lock().unwrap();
            // Bound speculative memory: beyond the cap, stop vouching for new blocks
            // (a round-scoped withhold) until commits drain the backlog. Re-verifying
            // an already-held block stays allowed — it adds nothing.
            if shared.pending.len() >= self.pending_cap && !shared.pending.contains(&block.digest())
            {
                warn!(
                    height = block.height_u64(),
                    pending = shared.pending.len(),
                    "speculative state at capacity (commits lagging?); withholding vote"
                );
                CONSENSUS_METRICS.verify_verdicts[&"withhold"].inc();
                return false;
            }
            shared
                .pending
                .branch_for_parent(parent.height_u64(), parent.digest())
        };
        let Some(branch) = branch else {
            // The parent's speculative state is not available (typically: this
            // validator restarted and lost its pending overlays). Withholding the vote
            // is safe; if the network finalizes the block, the commit path re-executes
            // it from the durable chain.
            warn!(
                height = block.height_u64(),
                "parent state unavailable; withholding vote"
            );
            CONSENSUS_METRICS.verify_verdicts[&"withhold"].inc();
            return false;
        };

        match self.execute_record(branch, record).await {
            Ok(output) => {
                let mut shared = self.shared.lock().unwrap();
                let overlay = Self::overlay_of(&output, record);
                shared.pending.insert(
                    block.digest(),
                    block.height_u64(),
                    {
                        use commonware_consensus::Block as _;
                        block.parent()
                    },
                    overlay,
                );
                shared.outputs.insert(block.digest(), output);
                CONSENSUS_METRICS.verify_verdicts[&"valid"].inc();
                CONSENSUS_METRICS
                    .speculative_blocks
                    .set(shared.pending.len() as u64);
                true
            }
            Err(reason) => {
                warn!(
                    height = block.height_u64(),
                    reason, "block failed re-execution; rejecting"
                );
                CONSENSUS_METRICS.verify_verdicts[&"invalid"].inc();
                false
            }
        }
    }

    async fn has_state(&mut self, block: &ConsensusBlock) -> bool {
        // Exactly the state question children ask: can a branch be resolved on it?
        self.shared
            .lock()
            .unwrap()
            .pending
            .branch_for_parent(block.height_u64(), block.digest())
            .is_some()
    }

    async fn committed_height(&mut self) -> Option<Height> {
        let height = self.shared.lock().unwrap().pending.committed().height;
        (height > 0).then(|| Height::new(height))
    }

    async fn adopt_committed_block(&mut self, block: &ConsensusBlock) {
        let mut shared = self.shared.lock().unwrap();
        let committed = shared.pending.committed();
        if block.height_u64() == committed.height {
            shared
                .pending
                .adopt_committed_digest(committed.height, block.digest());
        }
    }

    async fn commit(&mut self, block: ConsensusBlock) {
        let height = block.height_u64();
        // Record the height→digest mapping first thing: it is a pure finality fact
        // (true regardless of what happens to this delivery), idempotent under
        // redelivery, and the certificate written by the activity observer is only
        // reachable by height once this index exists.
        if let Some(finality) = &self.finality
            && let Err(err) = finality.index_height(
                height,
                block
                    .digest()
                    .as_ref()
                    .try_into()
                    .expect("sha256 digests are 32 bytes"),
            )
        {
            // Never block consensus on the auxiliary store: certificates remain
            // recoverable from the consensus archives while those exist, and the
            // certified watermark surfaces the gap loudly.
            error!(
                height,
                ?err,
                "failed to index finalized block in the finality store"
            );
        }
        let committed = {
            let shared = self.shared.lock().unwrap();
            if shared.pipeline_closed {
                warn!(height, "node is shutting down; dropping commit");
                return;
            }
            shared.pending.committed()
        };

        if height <= committed.height {
            // At-least-once delivery: consensus may re-deliver the tip after an
            // unclean shutdown. Anything below is impossible by construction (delivery
            // is ordered and starts above the durable floor).
            assert_eq!(
                height, committed.height,
                "consensus delivered a block below the durable chain"
            );
            if let Some(digest) = committed.digest {
                assert_eq!(
                    digest,
                    block.digest(),
                    "consensus re-delivered a different block at the committed height"
                );
            }
            return;
        }
        assert_eq!(
            height,
            committed.height + 1,
            "consensus delivered a block out of order"
        );

        let record = block
            .record()
            .expect("only genesis has no record and genesis is never delivered")
            .clone();

        // Take the output computed during build/verify, or re-execute against the
        // durable chain — the path taken for backfilled blocks and after restarts.
        let output = {
            let output = self.shared.lock().unwrap().outputs.remove(&block.digest());
            match output {
                Some(output) => output,
                None => {
                    let empty_branch = BranchOverrides::empty();
                    match self.execute_record(empty_branch, &record).await {
                        Ok(output) => output,
                        Err(reason) => {
                            // A finalized block that does not reproduce its declared
                            // outcome means this node's state diverged from the
                            // network's (or the chain finalized garbage, which honest
                            // quorums prevent). There is no way to continue.
                            error!(height, reason, "finalized block failed re-execution");
                            panic!("finalized block {height} failed re-execution: {reason}");
                        }
                    }
                }
            }
        };

        let committed_el_hash = output.as_ref().header.hash();
        // The mempool follows finality: included transactions leave it here, once, in
        // final order — never during speculative building.
        if let Some(builder) = self.builder.clone() {
            builder
                .lock()
                .await
                .on_committed(
                    output.as_ref().header.clone(),
                    &output.as_ref().account_diffs,
                    &record,
                )
                .await;
        }

        // Hand the block to the persistence pipeline and wait until it is durable
        // (in the write-ahead log). Consensus acknowledges — and allows more blocks
        // through — only after this returns: the node is the pacer.
        //
        // The pipeline disappearing means the node is shutting down (its own guards
        // make a mid-flight pipeline death fatal to the process). Returning without
        // the durability wait is safe here: nothing below persists an acknowledgement,
        // and after a restart consensus re-delivers from the durable height anyway.
        let sent = self
            .committed_sink
            .send(BlockPayload {
                output,
                record,
                // Finalized blocks re-enter the pipeline the way replayed blocks
                // always have; mempool bookkeeping belongs to the build path.
                command_type: BlockCommandType::Replay,
                failed_transactions: Vec::new(),
            })
            .await;
        if sent.is_err() {
            warn!(height, "persistence pipeline closed; dropping commit");
            self.shared.lock().unwrap().pipeline_closed = true;
            return;
        }
        let mut applied = self.applied.clone();
        if applied
            .wait_for(|number| number.is_some_and(|n| n >= height))
            .await
            .is_err()
        {
            warn!(height, "applier watch closed; dropping commit");
            self.shared.lock().unwrap().pipeline_closed = true;
            return;
        }

        let mut shared = self.shared.lock().unwrap();
        shared.pending.advance_committed(height, block.digest());
        shared.committed_el_hash = Some(committed_el_hash);
        let Shared {
            pending, outputs, ..
        } = &mut *shared;
        outputs.retain(|digest, _| pending.contains(digest));
        CONSENSUS_METRICS.committed_height.set(height);
        CONSENSUS_METRICS
            .last_commit_unix
            .set((millis_since_epoch() / 1000) as u64);
        CONSENSUS_METRICS
            .speculative_blocks
            .set(pending.len() as u64);
    }
}
