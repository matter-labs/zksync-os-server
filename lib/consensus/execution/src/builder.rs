//! Block building for consensus leaders.
//!
//! Mirrors the node's existing block-production semantics — same mempool streams, same
//! seal policy, same fee sourcing, same protocol-upgrade handling — with one structural
//! difference: production state does not come from a linear "last block" but from
//! whatever parent consensus asks us to build on (which may itself still be unfinalized).
//!
//! Mempool interaction under speculation, and why it is safe:
//!
//! - Building never consumes transactions from the pool (`strict_subpool_cleanup` stays
//!   off; the pool is updated once per block at *commit* time, in final order). An
//!   abandoned proposal therefore leaves the pool untouched.
//! - When building on an unfinalized parent, the pool may re-offer transactions the
//!   parent already included (it has not seen the parent commit yet). Execution rejects
//!   them against the parent's speculative state (nonce too low) and building continues
//!   with the rest — wasteful in the rare case, never incorrect.
//! - Transactions rejected during building are purged from the pool based on execution
//!   against the *parent branch's* state. If competing branches diverge, a transaction
//!   valid on another branch can be purged early — a liveness pinprick (the sender
//!   resubmits), never a safety issue.

use commonware_cryptography::sha256::Digest;
use futures::StreamExt as _;
use tokio::time::Instant;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_mempool::{MarkingTxStream, Pool, StreamOutcome};
use zksync_os_observability::ComponentStateReporter;
use zksync_os_sequencer::execution::block_context_provider::millis_since_epoch;
use zksync_os_sequencer::execution::execute_block_in_vm::execute_block_in_vm;
use zksync_os_sequencer::execution::{FeeParams, FeeProvider};
use zksync_os_sequencer::model::blocks::{
    BlockOutputWithReads, InvalidTxPolicy, PreparedBlockCommand, SealPolicy,
};
use zksync_os_storage_api::{BlockContext, BlockHashes, ReplayRecord, ViewState};
use zksync_os_types::{
    BlockStartCursors, ExecutionVersion, ProtocolSemanticVersion, SystemTxEnvelope, ZkEnvelope,
    ZkTransaction,
};

/// Everything about the parent that block production needs. Resolved by the execution
/// environment from the parent consensus block plus its own bookkeeping (pending
/// entries, the committed tip, or the genesis anchor).
#[derive(Debug, Clone)]
pub struct ParentInfo {
    pub number: u64,
    pub timestamp: u64,
    /// Execution-layer hash of the parent block (extends the block-hash ring).
    pub el_hash: alloy::primitives::B256,
    /// The parent's block-hash ring (not yet including the parent itself).
    pub block_hashes: BlockHashes,
    pub protocol_version: ProtocolSemanticVersion,
    /// L1-source cursors after the parent block.
    pub next_cursors: BlockStartCursors,
    /// Whether the parent block itself contains a protocol-upgrade transaction. Used to
    /// tell a stale pool offer apart from an upgrade that still belongs on this branch:
    /// the pool only unlists an upgrade at commit time, while building runs ahead of
    /// commits.
    pub carries_upgrade_tx: bool,
    /// The parent's fee parameters — the base for this block's fee clamps. Building
    /// clamps against the actual parent (not the last *finalized* block) so verifiers
    /// can hold proposals to the exact per-block fee movement rules.
    pub fee_params: FeeParams,
    /// Consensus digest of the parent (children's speculative state key).
    pub digest: Digest,
}

/// Static chain parameters for building; the consensus-mode counterpart of the block
/// context provider's configuration.
#[derive(Debug, Clone)]
pub struct BuilderConfig {
    pub l2_chain_id: u64,
    /// The chain id of the settlement layer. Fixed for the lifetime of the process:
    /// settlement-layer migration while consensus is running is not supported yet.
    pub sl_chain_id: u64,
    pub gas_limit: u64,
    pub pubdata_limit: u64,
    pub fee_collector_address: alloy::primitives::Address,
    /// How long the block stays open for transactions (counted from the start of
    /// execution; an idle chain seals empty blocks on this cadence).
    pub block_time: std::time::Duration,
    /// How long to wait for an idle mempool to offer a first transaction before
    /// building the block from whatever is at hand (possibly nothing).
    /// `idle_block_deadline + block_time` must stay well below the consensus leader
    /// timeout, or idle leader turns start expiring.
    pub idle_block_deadline: std::time::Duration,
    pub max_transactions_in_block: usize,
    pub interop_roots_per_block: u64,
}

/// Owns the mempool and fee sourcing; produces executed blocks on request.
pub struct ConsensusBlockBuilder<Subpool> {
    pool: Pool<Subpool>,
    fee_provider: FeeProvider,
    config: BuilderConfig,
    reporter: ComponentStateReporter,
}

/// A successfully built block: the replayable record plus its execution artifacts.
pub struct BuiltBlock {
    pub record: ReplayRecord,
    pub output: BlockOutputWithReads,
    pub next_cursors: BlockStartCursors,
}

impl<Subpool: L2Subpool> ConsensusBlockBuilder<Subpool> {
    pub fn new(pool: Pool<Subpool>, fee_provider: FeeProvider, config: BuilderConfig) -> Self {
        Self {
            pool,
            fee_provider,
            config,
            reporter: ComponentStateReporter::new("consensus_builder").0,
        }
    }

    /// The pool handle, for the commit-time canonical-state update.
    pub fn pool(&self) -> &Pool<Subpool> {
        &self.pool
    }

    /// Builds and executes one block on top of `parent`, reading state through `view`
    /// (the parent branch's speculative view). Returns `None` when nothing can be
    /// built — the caller passes its leader turn and consensus moves on.
    pub async fn build<V: ViewState + 'static>(
        &mut self,
        parent: &ParentInfo,
        view: V,
    ) -> Option<BuiltBlock> {
        match self.try_build(parent, view).await {
            Ok(built) => Some(built),
            Err(error) => {
                tracing::warn!(?error, parent = parent.number, "block building failed");
                None
            }
        }
    }

    async fn try_build<V: ViewState + 'static>(
        &mut self,
        parent: &ParentInfo,
        view: V,
    ) -> anyhow::Result<BuiltBlock> {
        let block_number = parent.number + 1;
        let fee_params = self
            .fee_provider
            .produce_fee_params_on(parent.fee_params)
            .await?;
        self.pool
            .update_pending_block_fees(fee_params.eip1559_basefee.saturating_to(), None);

        // TODO(consensus): interop traffic is not supported under consensus yet, so
        // interop system transactions are held back indefinitely (the corresponding
        // cursors never advance; see the cursor derivation in the environment).
        // Supporting it needs cursor advancement + verification rules for interop
        // roots/fee updates — the same shape as L1-priority-transaction authenticity.
        let far_future = Instant::now() + std::time::Duration::from_secs(60 * 60 * 24 * 365);
        // The pool hands out a transaction stream only once it has something to offer —
        // an await that never resolves while the chain is idle. A leader turn must not
        // hang on that: past the idle deadline, proceed with a stream that never yields
        // and let the seal policy close the block empty at its own deadline.
        let best_txs = match tokio::time::timeout(
            self.config.idle_block_deadline,
            self.pool.best_transactions_stream(far_future, false),
        )
        .await
        {
            Ok(available) => available.ok_or_else(|| anyhow::anyhow!("mempool is closed"))?,
            Err(_idle) => StreamOutcome {
                // No upgrade can be pending: the pool reports upgrades through the same
                // call, and it would have resolved immediately with one.
                upgrade_metadata: None,
                stream: MarkingTxStream::unmarkable(futures::stream::pending()),
            },
        };

        // Proposer-chosen timestamp: wall clock, but never behind the parent (virtual
        // or real clock skews must not produce a non-monotonic chain).
        let timestamp = ((millis_since_epoch() / 1000) as u64).max(parent.timestamp + 1);

        // Protocol upgrades ride the same path as in linear production: if an upgrade
        // transaction is pending and newer than the parent's protocol version, this
        // block carries it (and its forced preimages).
        let (protocol_version, force_preimages) = match best_txs.upgrade_metadata {
            Some(upgrade_metadata)
                if upgrade_metadata.protocol_version > parent.protocol_version =>
            {
                tracing::info!(
                    block_number,
                    ?upgrade_metadata,
                    "including protocol upgrade transaction in the block"
                );
                anyhow::ensure!(
                    upgrade_metadata.timestamp <= timestamp.saturating_add(5),
                    "upgrade transaction with timestamp {} received too early at {timestamp}",
                    upgrade_metadata.timestamp,
                );
                (
                    upgrade_metadata.protocol_version,
                    upgrade_metadata.force_preimages.clone(),
                )
            }
            Some(upgrade_metadata) if parent.carries_upgrade_tx => {
                // The pool follows finality while building runs ahead of it: right
                // after an upgrade block, the pool still offers the same upgrade until
                // the commit lands — and the stream then carries the upgrade
                // transaction itself, which must not execute twice. Pass this leader
                // turn; the in-flight commit clears the pool within a view.
                //
                // (An upgrade buried deeper in uncommitted ancestors is not detected
                // here; re-executing it simply fails the build, which is the same
                // pass-the-turn outcome.)
                anyhow::bail!(
                    "pool still offers upgrade {} already carried by the parent block",
                    upgrade_metadata.protocol_version,
                );
            }
            Some(_) => {
                // An upgrade at (or below) the chain's current version that no ancestor
                // included yet — the fresh-chain genesis upgrade is the canonical case.
                // Its transaction rides the stream and must execute; the metadata (a
                // version bump plus forced preimages) does not apply.
                (parent.protocol_version.clone(), Vec::new())
            }
            None => (parent.protocol_version.clone(), Vec::new()),
        };
        let execution_version: ExecutionVersion = (&protocol_version)
            .try_into()
            .map_err(|_| anyhow::anyhow!("unsupported execution version {protocol_version}"))?;

        // The one-time SetSLChainId system transaction, exactly as in linear
        // production: appended when the chain first reaches protocol v31 (which for a
        // fresh consensus chain is its first block).
        let (tx_source, expect_sl_chain_id_tx_after_upgrade) = if protocol_version.minor == 31
            && (parent.protocol_version.minor < 31 || parent.number == 0)
        {
            let sl_chain_id_tx = SystemTxEnvelope::set_sl_chain_id(
                self.config.sl_chain_id,
                // Placeholder: not an actual migration.
                u64::MAX,
            );
            let tx_source =
                zksync_os_mempool::MarkingTxStream::unmarkable(best_txs.stream.stream.chain(
                    futures::stream::once(async move { ZkTransaction::from(sl_chain_id_tx) }),
                ));
            (tx_source, true)
        } else {
            (best_txs.stream, false)
        };

        // The pool follows finality while building runs ahead of it: L1 priority
        // transactions the (still uncommitted) ancestors already included are re-offered
        // until the commits land. Priority ids are strictly ordered, so anything below
        // the parent branch's cursor is by definition already included — drop it here.
        // (L2 transactions need no such filter: re-offered ones fail nonce checks in
        // execution and are skipped there.)
        let mut tx_source = tx_source;
        let branch_l1_cursor = parent.next_cursors.l1_priority_id;
        tx_source.stream = tx_source
            .stream
            .filter(move |tx| {
                let keep = match tx.envelope() {
                    ZkEnvelope::L1(l1_tx) => l1_tx.priority_id() >= branch_l1_cursor,
                    _ => true,
                };
                if !keep {
                    tracing::debug!(
                        branch_l1_cursor,
                        "dropping L1 transaction already included on the parent branch"
                    );
                }
                futures::future::ready(keep)
            })
            .boxed();

        let block_context = BlockContext {
            eip1559_basefee: fee_params.eip1559_basefee,
            native_price: fee_params.native_price,
            pubdata_price: fee_params.pubdata_price,
            block_number,
            timestamp,
            chain_id: self.config.l2_chain_id,
            coinbase: self.config.fee_collector_address,
            block_hashes: parent.block_hashes.push(parent.el_hash),
            gas_limit: self.config.gas_limit,
            pubdata_limit: self.config.pubdata_limit,
            mix_hash: Default::default(),
            execution_version: execution_version as u32,
            blob_fee: alloy::primitives::U256::ONE,
        };

        let command = PreparedBlockCommand {
            block_context,
            tx_source,
            // Cadence, not Decide: the deadline runs from the start of execution, so an
            // idle chain seals empty blocks on schedule instead of stalling the view.
            seal_policy: SealPolicy::Cadence(
                self.config.block_time,
                self.config.max_transactions_in_block,
            ),
            invalid_tx_policy: InvalidTxPolicy::RejectAndContinue {
                mark_in_source: true,
            },
            metrics_label: "consensus_build",
            protocol_version,
            expected_block_output_hash: None,
            previous_block_timestamp: parent.timestamp,
            force_preimages,
            expect_sl_chain_id_tx_after_upgrade,
            starting_cursors: parent.next_cursors.clone(),
            interop_roots_per_block: self.config.interop_roots_per_block,
            // Never consume from the pool while building: the block may be abandoned.
            // The pool learns about included transactions at commit time.
            strict_subpool_cleanup: false,
        };

        let (output, record, _failed, _) = execute_block_in_vm(
            command,
            view,
            &self.reporter,
            zksync_os_interface::tracing::NopTracer,
            zksync_os_interface::tracing::NopValidator,
        )
        .await
        .map_err(|dump| anyhow::anyhow!("build execution failed: {}", dump.error))?;

        let next_cursors = derive_next_cursors(&record);
        tracing::debug!(
            block_number,
            transactions = record.transactions.len(),
            "built block proposal"
        );
        Ok(BuiltBlock {
            record,
            output,
            next_cursors,
        })
    }
}

/// L1-source cursors after a block, derived purely from its content.
///
/// Supported today: L1 priority transactions (the cursor advances past the highest
/// included priority id). Interop roots, migrations, and interop fee updates are not
/// yet supported under consensus, so their cursors pass through unchanged — consensus
/// chains must run with interop disabled until cursor advancement (and the matching
/// verification rules) are implemented for them.
pub fn derive_next_cursors(record: &ReplayRecord) -> BlockStartCursors {
    let mut cursors = record.starting_cursors.clone();
    for tx in &record.transactions {
        if let ZkEnvelope::L1(l1_tx) = tx.envelope() {
            cursors.l1_priority_id = cursors.l1_priority_id.max(l1_tx.priority_id() + 1);
        }
    }
    cursors
}

/// The builder interface the execution environment drives. One production
/// implementation exists ([`ConsensusBlockBuilder`]); the indirection keeps the
/// environment generic only over the state backend and lets tests run without a
/// mempool (no builder = this validator never proposes).
#[async_trait::async_trait]
pub trait BuildBlocks<V: ViewState + 'static>: Send {
    /// Build one block on `parent`, reading state through `view`. `None` = pass the
    /// leader turn.
    async fn build_block(&mut self, parent: &ParentInfo, view: V) -> Option<BuiltBlock>;

    /// A block was finalized and durably applied: update the mempool (drop included
    /// transactions, advance nonces, release stale descendants).
    async fn on_committed(
        &mut self,
        header: alloy::consensus::Sealed<alloy::consensus::Header>,
        account_diffs: &[zksync_os_interface::types::AccountDiff],
        record: &ReplayRecord,
    );
}

#[async_trait::async_trait]
impl<V, Subpool> BuildBlocks<V> for ConsensusBlockBuilder<Subpool>
where
    V: ViewState + 'static,
    Subpool: L2Subpool + Send,
{
    async fn build_block(&mut self, parent: &ParentInfo, view: V) -> Option<BuiltBlock> {
        self.build(parent, view).await
    }

    async fn on_committed(
        &mut self,
        header: alloy::consensus::Sealed<alloy::consensus::Header>,
        account_diffs: &[zksync_os_interface::types::AccountDiff],
        record: &ReplayRecord,
    ) {
        // Replay-style update: included transactions leave the pool because the state
        // (nonces, balances) moved past them, not because this node built the block.
        self.pool
            .on_canonical_state_change(header, account_diffs, record, false)
            .await;
    }
}
