//! The node's execution environment for consensus.
//!
//! This is where consensus meets the real node: blocks are verified by re-executing
//! them through the production VM against speculative state, and finalized blocks are
//! handed to the node's persistence pipeline (write-ahead log first, then state,
//! repositories, tree) with consensus paced by durability.
//!
//! Scope note — currently implemented: the follower/replica half (verify, commit,
//! genesis, committed height). Block *building* (the leader half: mempool integration,
//! seal policy, fee and cursor sourcing) is not wired yet; until it is, `build` declines
//! to propose, which consensus treats as this validator passing its turn as leader.

use crate::block::ConsensusBlock;
use crate::pending_state::{BranchOverrides, CommittedHead, Overlay, PendingState};
use commonware_consensus::types::Height;
use commonware_cryptography::Digestible;
use commonware_cryptography::sha256::Digest;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use tracing::{error, warn};
use zksync_os_consensus_core::{BuildContext, ExecutionEnv};
use zksync_os_interface::tracing::{NopTracer, NopValidator};
use zksync_os_interface::traits::{PreimageSource, ReadStorage};
use zksync_os_mempool::MarkingTxStream;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_sequencer::execution::execute_block_in_vm::execute_block_in_vm;
use zksync_os_sequencer::model::blocks::{
    BlockCommandType, BlockOutputWithReads, BlockPayload, InvalidTxPolicy, PreparedBlockCommand,
    SealPolicy,
};
use zksync_os_storage_api::state_override_view::OverriddenStateView;
use zksync_os_storage_api::{ReadStateHistory, ReplayRecord};
use zksync_os_types::{SystemTxType, ZkEnvelope};

/// Chain-level constants the environment needs to anchor the chain root.
#[derive(Debug, Clone, Copy)]
pub struct ChainAnchor {
    /// Hash of the genesis block header.
    pub genesis_block_hash: alloy::primitives::B256,
    /// Timestamp of the genesis block.
    pub genesis_timestamp: u64,
}

/// A self-contained read view of the node state at a fixed block height.
///
/// The backend's native views borrow from the backend handle, which would tie them to a
/// lock held across VM execution. This wrapper owns a backend handle (cheap to clone)
/// and opens a fresh view per read — view construction is O(1) on the default backend.
#[derive(Clone)]
struct BaseViewAt<S> {
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

/// What the environment hands the node's persistence pipeline per finalized block.
/// Identical to what the pre-consensus pipeline's canonization fence emitted, so the
/// existing applier tail consumes it unchanged.
pub type CommittedPayload = BlockPayload;

#[derive(Clone)]
pub struct NodeExecutionEnv<S> {
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
}

struct Shared {
    pending: PendingState,
    /// Execution outputs of pending blocks, kept for the eventual commit so finalized
    /// blocks are not re-executed on the happy path.
    outputs: HashMap<Digest, BlockOutputWithReads>,
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
        committed_sink: mpsc::Sender<CommittedPayload>,
        applied: watch::Receiver<Option<u64>>,
        interop_roots_per_block: u64,
    ) -> Self {
        let genesis = ConsensusBlock::genesis(anchor.genesis_block_hash);
        let committed = CommittedHead {
            height: committed_height,
            // Consensus digests are not persisted in the node's storage; after a
            // restart above genesis, the committed tip's digest is unknown and parents
            // at that height are matched by height (consensus has already validated
            // their ancestry).
            digest: (committed_height == 0).then(|| genesis.digest()),
        };
        Self {
            base,
            anchor,
            genesis,
            shared: Arc::new(Mutex::new(Shared {
                pending: PendingState::new(committed),
                outputs: HashMap::new(),
            })),
            committed_sink,
            applied,
            reporter: ComponentStateReporter::new("consensus_execution").0,
            interop_roots_per_block,
        }
    }

    /// Structural consistency between a block's context and its parent — the same
    /// invariants the node's replay path enforces on blocks arriving from outside.
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
        let view = OverriddenStateView::new(
            BaseViewAt {
                base: self.base.clone(),
                height: committed_height,
            },
            branch,
        );

        let expect_sl_chain_id_tx_after_upgrade = record.transactions.windows(2).any(|window| {
            matches!(window[0].envelope(), ZkEnvelope::Upgrade(_))
                && matches!(
                    window[1].as_system_tx_type(),
                    Some(SystemTxType::SetSLChainId(_, _))
                )
        });

        let command = PreparedBlockCommand {
            block_context: record.block_context,
            seal_policy: SealPolicy::UntilExhausted {
                allowed_to_finish_early: false,
            },
            invalid_tx_policy: InvalidTxPolicy::Abort,
            tx_source: MarkingTxStream::unmarkable(futures::stream::iter(
                record.transactions.clone(),
            )),
            metrics_label: "consensus_verify",
            protocol_version: record.protocol_version.clone(),
            expected_block_output_hash: Some(record.block_output_hash),
            previous_block_timestamp: record.previous_block_timestamp,
            force_preimages: record.force_preimages.clone(),
            expect_sl_chain_id_tx_after_upgrade,
            starting_cursors: record.starting_cursors.clone(),
            interop_roots_per_block: self.interop_roots_per_block,
            strict_subpool_cleanup: false,
        };

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
        _parent: ConsensusBlock,
        _context: BuildContext,
    ) -> Option<ConsensusBlock> {
        // Block building (mempool, seal policy, fee/cursor sourcing) is not wired yet.
        // Declining to propose is safe: consensus times the view out and moves to the
        // next leader.
        warn!("block building not implemented; passing this leader turn");
        None
    }

    async fn verify(&mut self, parent: ConsensusBlock, block: ConsensusBlock) -> bool {
        let Some(record) = block.record() else {
            warn!("received a payload-less block above genesis; rejecting");
            return false;
        };
        if let Err(reason) = self.check_linkage(&parent, record) {
            warn!(
                height = block.height_u64(),
                reason, "block failed linkage checks; rejecting"
            );
            return false;
        }
        let branch = {
            let shared = self.shared.lock().unwrap();
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
                true
            }
            Err(reason) => {
                warn!(
                    height = block.height_u64(),
                    reason, "block failed re-execution; rejecting"
                );
                false
            }
        }
    }

    async fn committed_height(&mut self) -> Option<Height> {
        let height = self.shared.lock().unwrap().pending.committed().height;
        (height > 0).then(|| Height::new(height))
    }

    async fn commit(&mut self, block: ConsensusBlock) {
        let height = block.height_u64();
        let committed = { self.shared.lock().unwrap().pending.committed() };

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

        // Hand the block to the persistence pipeline and wait until it is durable
        // (in the write-ahead log). Consensus acknowledges — and allows more blocks
        // through — only after this returns: the node is the pacer.
        self.committed_sink
            .send(BlockPayload {
                output,
                record,
                // Finalized blocks re-enter the pipeline the way replayed blocks
                // always have; mempool bookkeeping belongs to the build path.
                command_type: BlockCommandType::Replay,
                failed_transactions: Vec::new(),
            })
            .await
            .expect("persistence pipeline closed");
        let mut applied = self.applied.clone();
        applied
            .wait_for(|number| number.is_some_and(|n| n >= height))
            .await
            .expect("applier watch closed");

        let mut shared = self.shared.lock().unwrap();
        shared.pending.advance_committed(height, block.digest());
        let Shared { pending, outputs } = &mut *shared;
        outputs.retain(|digest, _| pending.contains(digest));
    }
}
