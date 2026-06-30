use crate::execution::fee_provider::{FeeParams, FeeProvider};
use crate::execution::metrics::EXECUTION_METRICS;
use crate::model::blocks::{
    BlockCommand, InvalidTxPolicy, PreparedBlockCommand, RebuildCommand, SealPolicy,
};
use alloy::primitives::{Address, BlockHash, TxHash, U256};
use anyhow::Context as _;
use futures::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};
use zksync_os_contract_interface::settlement_layer_intervals::{
    IntervalSettlementLayer, SettlementLayerIntervals,
};
use zksync_os_genesis::genesis_header;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_mempool::{MarkingTxStream, Pool};
use zksync_os_storage_api::BlockContext;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{
    BlockOutput, BlockStartCursors, ExecutionVersion, SystemTxEnvelope, SystemTxType, ZkEnvelope,
    ZkTransaction,
};

/// Component that turns `BlockCommand`s into `PreparedBlockCommand`s.
/// Last step in the stream where `Produce` and `Replay` are differentiated.
///
///  * Tracks L1 priority ID and 256 previous block hashes.
///  * Combines the L1 and L2 transactions
///  * Cross-checks L1 transactions in Replay blocks against L1 (important for ENs) todo: not implemented yet
///
/// Note: unlike other components, this one doesn't tolerate replaying blocks -
///  it doesn't tolerate jumps in L1 priority IDs.
///  this is easily fixable if needed.
pub struct BlockContextProvider<Subpool> {
    fee_provider: FeeProvider,
    pool: Pool<Subpool>,
    config: Config,
    last_block: Option<LastBlock>,
    next_interop_tx_allowed_after: Instant,
    /// L2 chain id of the chain's currently-active settlement layer. Can change in runtime if there
    /// is a migration in the process.
    current_sl_chain_id: u64,
    last_constructed_block_ctx_sender: watch::Sender<Option<BlockContext>>,
    /// Test/bench-only: when present and `active`, block production bypasses the mempool and streams
    /// transactions directly from this channel instead of `pool.best_transactions_stream()`.
    direct_tx: Option<DirectTxSource>,
    /// Test/bench-only: per-signer overflow carried across `produce_parallel` rounds, so an uneven
    /// direct-injection feed never packs a block beyond `max_transactions_in_block` (which the VM
    /// would seal early on NativeCycles, dropping the excess and opening a nonce gap).
    direct_buffers: std::collections::HashMap<Address, std::collections::VecDeque<ZkTransaction>>,
    /// Test/bench-only: per-lane overflow for parallel direct injection. This avoids re-bucketing a
    /// shared channel by signer every round when the benchmark already knows each signer's lane.
    direct_lane_buffers: Vec<std::collections::VecDeque<ZkTransaction>>,
}

/// Test/bench-only handle that feeds transactions straight into block production, bypassing the
/// mempool. See [`BlockContextProvider`].
pub struct DirectTxSource {
    /// Wrapped in `Arc<Mutex<_>>` so each block's stream can own a clone (no borrow of `self`) while
    /// the receiver persists across blocks.
    pub rx: Arc<Mutex<mpsc::Receiver<ZkTransaction>>>,
    /// Optional per-signer receivers for parallel direct-injection load tests.
    pub lanes: Vec<Arc<Mutex<mpsc::Receiver<ZkTransaction>>>>,
    /// Direct injection only takes over once this is set, so the node can first replay genesis /
    /// apply the protocol upgrade / process the initial deposit through the normal mempool path
    /// (which blocks on an empty mempool, so it cannot be used for an empty bench stream).
    pub active: Arc<AtomicBool>,
}

pub struct Config {
    pub l2_chain_id: u64,
    pub l1_chain_id: u64,
    pub gas_limit: u64,
    pub pubdata_limit: u64,
    pub fee_collector_address: Address,
    pub block_time: Duration,
    pub service_block_delay: Duration,
    pub max_transactions_in_block: usize,
    pub interop_roots_per_block: u64,
}

struct LastBlock {
    record: ReplayRecord,
    hash: BlockHash,
    next_cursors: BlockStartCursors,
}

impl<Subpool: L2Subpool> BlockContextProvider<Subpool> {
    pub fn new(
        fee_provider: FeeProvider,
        pool: Pool<Subpool>,
        config: Config,
        intervals: &SettlementLayerIntervals,
        last_constructed_block_ctx_sender: watch::Sender<Option<BlockContext>>,
        direct_tx: Option<DirectTxSource>,
    ) -> Self {
        let current_sl_chain_id = match intervals.current_settlement_layer() {
            IntervalSettlementLayer::L1 => config.l1_chain_id,
            IntervalSettlementLayer::Gateway(gw_chain_id) => *gw_chain_id,
        };
        Self {
            fee_provider,
            pool,
            config,
            last_block: None,
            next_interop_tx_allowed_after: Instant::now(),
            current_sl_chain_id,
            last_constructed_block_ctx_sender,
            direct_tx,
            direct_buffers: std::collections::HashMap::new(),
            direct_lane_buffers: Vec::new(),
        }
    }

    /// `true` when the chain currently settles on a Gateway (i.e. its tracked SL chain id
    /// differs from L1's).
    fn settles_on_gateway(&self) -> bool {
        self.current_sl_chain_id != self.config.l1_chain_id
    }

    pub fn last_block_number(&self) -> Option<u64> {
        self.last_block
            .as_ref()
            .map(|b| b.record.block_context.block_number)
    }

    pub async fn prepare_command(
        &mut self,
        block_command: BlockCommand,
    ) -> anyhow::Result<Option<PreparedBlockCommand<'_>>> {
        match block_command {
            BlockCommand::Produce(_) => self.produce().await,
            BlockCommand::Replay(record) => self.replay(record).await,
            BlockCommand::Rebuild(rebuild) => self.rebuild(rebuild).await,
        }
    }

    async fn produce(&mut self) -> anyhow::Result<Option<PreparedBlockCommand<'_>>> {
        let LastBlock {
            record: previous_record,
            hash: previous_block_hash,
            next_cursors,
        } = self
            .last_block
            .take()
            .expect("tried to produce a block without replaying at least one record");
        let fee_params = self.fee_provider.produce_fee_params().await?;
        self.pool
            .update_pending_block_fees(fee_params.eip1559_basefee.saturating_to(), None);
        let block_number = previous_record.block_context.block_number + 1;
        // Create stream:
        // - If available, upgrade tx goes first (expected to be the only tx in the block, enforced by sequencer).
        // - L1 transactions first, then L2 transactions.
        // Obtain the base transaction stream and any upgrade metadata. In direct-injection mode
        // (test/bench only) we bypass the mempool entirely and stream transactions straight from
        // the injected channel.
        let use_direct = self
            .direct_tx
            .as_ref()
            .is_some_and(|d| d.active.load(Ordering::Relaxed));
        let (upgrade_metadata, pool_stream) = if use_direct {
            (None, None)
        } else {
            let best_txs = self
                .pool
                .best_transactions_stream(
                    self.next_interop_tx_allowed_after,
                    self.settles_on_gateway(),
                )
                .await
                .context("mempool is closed")?;
            (best_txs.upgrade_metadata, Some(best_txs.stream))
        };

        let timestamp = (millis_since_epoch() / 1000) as u64;

        // Check if we peeked an upgrade transaction info.
        // It is possible that we peek an upgrade with version <= self.protocol_version
        // since we do not consume patch upgrades when replaying/rebuilding blocks. Such upgrade can be safely skipped.
        let (protocol_version, force_preimages) = if let Some(upgrade_metadata) = upgrade_metadata
            && upgrade_metadata.protocol_version > previous_record.protocol_version
        {
            tracing::info!(
                block_number,
                ?upgrade_metadata,
                "including protocol upgrade transaction in the block"
            );
            // Invariant: transactions sent through this stream must be ready for execution, e.g.
            // transaction should not be sent until timestamp is reached.
            // We add some margin of error for timestamp comparison.
            let current_timestamp = timestamp.saturating_add(5);
            anyhow::ensure!(
                upgrade_metadata.timestamp <= current_timestamp,
                "upgrade transaction with timestamp {} received too early at {}; tx: {upgrade_metadata:?}",
                upgrade_metadata.timestamp,
                current_timestamp
            );
            (
                upgrade_metadata.protocol_version,
                upgrade_metadata.force_preimages.clone(),
            )
        } else {
            (previous_record.protocol_version.clone(), Vec::new())
        };

        let execution_version: ExecutionVersion = (&protocol_version)
            .try_into()
            .context("Cannot instantiate a block for unsupported execution version")?;

        // Append a SetSLChainId system transaction exactly once: when the protocol
        // version is v31 (either via upgrade from v30, or on the first block of a
        // fresh v31 chain). After it fires once, the condition can never trigger again.
        let expect_sl_chain_id_tx_after_upgrade = protocol_version.minor == 31
            && (previous_record.protocol_version.minor < 31
                || previous_record.block_context.block_number == 0);
        // `u64::MAX` is a placeholder, since this is not an actual migration.
        let sl_chain_id_tx = SystemTxEnvelope::set_sl_chain_id(self.current_sl_chain_id, u64::MAX);

        let tx_source = match pool_stream {
            Some(stream) => {
                if expect_sl_chain_id_tx_after_upgrade {
                    MarkingTxStream::unmarkable(stream.stream.chain(futures::stream::once(
                        async move { ZkTransaction::from(sl_chain_id_tx) },
                    )))
                } else {
                    stream
                }
            }
            None => {
                // Direct injection: the stream owns an `Arc` clone of the receiver (so it does not
                // borrow `self` and survives across blocks) and is pending when the channel is
                // empty, letting the block seal on its deadline.
                let rx = self
                    .direct_tx
                    .as_ref()
                    .expect("direct_tx present")
                    .rx
                    .clone();
                let direct = futures::stream::poll_fn(move |cx| rx.lock().unwrap().poll_recv(cx));
                if expect_sl_chain_id_tx_after_upgrade {
                    MarkingTxStream::unmarkable(direct.chain(futures::stream::once(async move {
                        ZkTransaction::from(sl_chain_id_tx)
                    })))
                } else {
                    MarkingTxStream::unmarkable(direct)
                }
            }
        };

        let FeeParams {
            eip1559_basefee,
            native_price,
            pubdata_price,
        } = fee_params;
        let block_context = BlockContext {
            eip1559_basefee,
            native_price,
            pubdata_price,
            block_number,
            timestamp,
            chain_id: self.config.l2_chain_id,
            coinbase: self.config.fee_collector_address,
            block_hashes: previous_record
                .block_context
                .block_hashes
                .push(previous_block_hash),
            gas_limit: self.config.gas_limit,
            pubdata_limit: self.config.pubdata_limit,
            // todo: initialize as source of randomness, i.e. the value of prevRandao
            mix_hash: Default::default(),
            execution_version: execution_version as u32,
            blob_fee: U256::ONE,
        };
        self.last_constructed_block_ctx_sender
            .send_replace(Some(block_context));
        Ok(Some(PreparedBlockCommand {
            block_context,
            tx_source,
            seal_policy: SealPolicy::Decide(
                self.config.block_time,
                self.config.max_transactions_in_block,
            ),
            invalid_tx_policy: InvalidTxPolicy::RejectAndContinue {
                // The direct-injection channel is an unmarkable stream, so invalid txs must be
                // dropped without trying to mark them back in the (non-existent) source pool.
                mark_in_source: !use_direct,
            },
            metrics_label: "produce",
            protocol_version,
            expected_block_output_hash: None,
            previous_block_timestamp: previous_record.block_context.timestamp,
            force_preimages,
            expect_sl_chain_id_tx_after_upgrade,
            starting_cursors: next_cursors.clone(),
            interop_roots_per_block: self.config.interop_roots_per_block,
            strict_subpool_cleanup: true,
            // Pipeline the VM only for plain direct-injection blocks. A block that still carries the
            // one-off SetSLChainId system tx must seal specially, which the pipelined loop doesn't
            // handle, so fall back to the serial loop there.
            direct_injection: use_direct && !expect_sl_chain_id_tx_after_upgrade,
        }))
    }

    /// `true` when direct injection (test/bench only) is configured and currently active.
    pub fn is_direct_active(&self) -> bool {
        self.direct_tx
            .as_ref()
            .is_some_and(|d| d.active.load(Ordering::Relaxed))
    }

    /// Bench-only: build up to `k` slot-disjoint blocks for one parallel round.
    ///
    /// Drains a batch (`k * max_transactions_in_block`) of direct-injection transactions and buckets
    /// them **by sender** into disjoint groups; each group becomes one block (numbered `N..N+K-1`),
    /// sealed on stream exhaustion. All blocks share the previous block's base hash ring (no
    /// inter-block hash chaining - the bench disregards chain validity, and native transfers don't
    /// read `BLOCKHASH`), so the contexts are built up front and the blocks can execute in parallel
    /// against the same base state at `N-1`. `last_block` is taken here and re-established by the
    /// caller via `on_canonical_state_change` for each produced block, in order.
    ///
    /// Requires direct injection to be active. Ignores `max_blocks_to_produce` (bench).
    pub async fn produce_parallel(
        &mut self,
        k: usize,
    ) -> anyhow::Result<Vec<PreparedBlockCommand<'static>>> {
        let LastBlock {
            record: previous_record,
            hash: previous_block_hash,
            next_cursors,
        } = self
            .last_block
            .take()
            .expect("tried to produce a block without replaying at least one record");
        let fee_params = self.fee_provider.produce_fee_params().await?;
        self.pool
            .update_pending_block_fees(fee_params.eip1559_basefee.saturating_to(), None);
        let base_block_number = previous_record.block_context.block_number + 1;
        let base_timestamp = (millis_since_epoch() / 1000) as u64;
        let protocol_version = previous_record.protocol_version.clone();
        let execution_version: ExecutionVersion = (&protocol_version)
            .try_into()
            .context("Cannot instantiate a block for unsupported execution version")?;
        let base_ring = previous_record
            .block_context
            .block_hashes
            .push(previous_block_hash);

        let max_tx = self.config.max_transactions_in_block;
        let FeeParams {
            eip1559_basefee,
            native_price,
            pubdata_price,
        } = fee_params;
        let direct_lanes = self
            .direct_tx
            .as_ref()
            .expect("produce_parallel requires direct injection")
            .lanes
            .clone();
        if direct_lanes.len() >= k {
            if self.direct_lane_buffers.len() < direct_lanes.len() {
                self.direct_lane_buffers
                    .resize_with(direct_lanes.len(), Default::default);
            }

            let mut commands = Vec::with_capacity(k);
            for (lane_index, rx) in direct_lanes.into_iter().take(k).enumerate() {
                if self.direct_lane_buffers[lane_index].is_empty() {
                    // Park each empty lane until its pusher supplies at least one tx. The parallel
                    // benchmark has one pusher per lane, so no shared receiver drain/rebucket is
                    // needed on the hot path.
                    let Some(first) =
                        std::future::poll_fn(|cx| rx.lock().unwrap().poll_recv(cx)).await
                    else {
                        continue;
                    };
                    self.direct_lane_buffers[lane_index].push_back(first);
                }
                {
                    let queue = &mut self.direct_lane_buffers[lane_index];
                    let mut guard = rx.lock().unwrap();
                    while queue.len() < max_tx {
                        match guard.try_recv() {
                            Ok(tx) => queue.push_back(tx),
                            // Lane momentarily empty (reader behind) - emit what this lane has.
                            Err(_) => break,
                        }
                    }
                }

                let queue = &mut self.direct_lane_buffers[lane_index];
                if queue.is_empty() {
                    continue;
                }
                let take = queue.len().min(max_tx);
                let txs: Vec<ZkTransaction> = queue.drain(..take).collect();
                let i = commands.len();
                let block_context = BlockContext {
                    eip1559_basefee,
                    native_price,
                    pubdata_price,
                    block_number: base_block_number + i as u64,
                    timestamp: base_timestamp + i as u64,
                    chain_id: self.config.l2_chain_id,
                    coinbase: self.config.fee_collector_address,
                    // Same base ring for every block in the round - no inter-block chaining (bench).
                    block_hashes: base_ring,
                    gas_limit: self.config.gas_limit,
                    pubdata_limit: self.config.pubdata_limit,
                    mix_hash: Default::default(),
                    execution_version: execution_version as u32,
                    blob_fee: U256::ONE,
                };
                commands.push(PreparedBlockCommand {
                    block_context,
                    tx_source: MarkingTxStream::unmarkable(futures::stream::iter(txs)),
                    // Group is <= max_tx (< the NativeCycles seal), so the stream exhausts before
                    // the VM seals: no tx is dropped. `allowed_to_finish_early` accepts exhaustion.
                    seal_policy: SealPolicy::UntilExhausted {
                        allowed_to_finish_early: true,
                    },
                    invalid_tx_policy: InvalidTxPolicy::RejectAndContinue {
                        mark_in_source: false,
                    },
                    metrics_label: "produce_parallel",
                    protocol_version: protocol_version.clone(),
                    expected_block_output_hash: None,
                    previous_block_timestamp: previous_record.block_context.timestamp,
                    force_preimages: Vec::new(),
                    expect_sl_chain_id_tx_after_upgrade: false,
                    starting_cursors: next_cursors.clone(),
                    interop_roots_per_block: self.config.interop_roots_per_block,
                    strict_subpool_cleanup: false,
                    direct_injection: true,
                });
            }
            return Ok(commands);
        }

        // Refill the per-signer buffers from the direct channel. Carry-over from the previous round is
        // kept, so an uneven feed never forces a block past `max_tx` (which would seal early on
        // NativeCycles and drop the excess, opening a nonce gap). A bounded drain plus the channel's
        // own backpressure keep the buffers from growing without limit.
        let rx = self
            .direct_tx
            .as_ref()
            .expect("produce_parallel requires direct injection")
            .rx
            .clone();
        let buffered: usize = self.direct_buffers.values().map(|q| q.len()).sum();
        if buffered == 0 {
            // Park until the first tx arrives (avoids busy-looping on an empty channel); the lock is
            // released between polls, not held across the await.
            let Some(first) = std::future::poll_fn(|cx| rx.lock().unwrap().poll_recv(cx)).await
            else {
                return Ok(Vec::new()); // channel closed
            };
            self.direct_buffers
                .entry(first.signer())
                .or_default()
                .push_back(first);
        }
        {
            let mut guard = rx.lock().unwrap();
            let budget = k.saturating_mul(max_tx);
            let mut pulled = 0usize;
            while pulled < budget {
                match guard.try_recv() {
                    Ok(tx) => {
                        self.direct_buffers
                            .entry(tx.signer())
                            .or_default()
                            .push_back(tx);
                        pulled += 1;
                    }
                    // Channel momentarily empty (readers behind) - emit what we have buffered.
                    Err(_) => break,
                }
            }
        }

        // Emit up to `k` blocks, one signer each, capped at `max_tx`; the remainder (a signer's
        // overflow, or signers beyond `k`) stays buffered for the next round so each signer's nonces
        // stay contiguous.
        let mut commands = Vec::new();
        let signers: Vec<Address> = self.direct_buffers.keys().copied().collect();
        for signer in signers {
            if commands.len() >= k {
                break;
            }
            let queue = self
                .direct_buffers
                .get_mut(&signer)
                .expect("signer buffer present");
            if queue.is_empty() {
                continue;
            }
            let take = queue.len().min(max_tx);
            let txs: Vec<ZkTransaction> = queue.drain(..take).collect();
            let i = commands.len();
            let block_context = BlockContext {
                eip1559_basefee,
                native_price,
                pubdata_price,
                block_number: base_block_number + i as u64,
                timestamp: base_timestamp + i as u64,
                chain_id: self.config.l2_chain_id,
                coinbase: self.config.fee_collector_address,
                // Same base ring for every block in the round - no inter-block chaining (bench).
                block_hashes: base_ring,
                gas_limit: self.config.gas_limit,
                pubdata_limit: self.config.pubdata_limit,
                mix_hash: Default::default(),
                execution_version: execution_version as u32,
                blob_fee: U256::ONE,
            };
            commands.push(PreparedBlockCommand {
                block_context,
                tx_source: MarkingTxStream::unmarkable(futures::stream::iter(txs)),
                // Group is <= max_tx (< the NativeCycles seal), so the stream exhausts before the VM
                // seals: no tx is dropped. `allowed_to_finish_early` accepts the exhaustion seal.
                seal_policy: SealPolicy::UntilExhausted {
                    allowed_to_finish_early: true,
                },
                invalid_tx_policy: InvalidTxPolicy::RejectAndContinue {
                    mark_in_source: false,
                },
                metrics_label: "produce_parallel",
                protocol_version: protocol_version.clone(),
                expected_block_output_hash: None,
                previous_block_timestamp: previous_record.block_context.timestamp,
                force_preimages: Vec::new(),
                expect_sl_chain_id_tx_after_upgrade: false,
                starting_cursors: next_cursors.clone(),
                interop_roots_per_block: self.config.interop_roots_per_block,
                strict_subpool_cleanup: false,
                direct_injection: true,
            });
        }
        // Drop now-empty buffers so the map doesn't accumulate idle signers.
        self.direct_buffers.retain(|_, q| !q.is_empty());
        Ok(commands)
    }

    async fn replay(
        &mut self,
        record: Box<ReplayRecord>,
    ) -> anyhow::Result<Option<PreparedBlockCommand<'_>>> {
        if self.last_block.is_none() {
            // As this is the first block we are replaying, we need to initialize the mempool.
            self.pool.init(&record).await;
        }

        if record.block_context.block_number == 0 {
            self.last_block = Some(LastBlock {
                record: *record,
                hash: genesis_header().hash(),
                next_cursors: Default::default(),
            });
            return Ok(None);
        }

        if let Some(LastBlock {
            record: last_record,
            ..
        }) = &self.last_block
        {
            anyhow::ensure!(
                last_record.block_context.block_number + 1 == record.block_context.block_number,
                "blocks received our of order: last block was {}, but received {}",
                last_record.block_context.block_number,
                record.block_context.block_number
            );
            anyhow::ensure!(
                last_record.block_context.timestamp == record.previous_block_timestamp,
                "inconsistent previous block timestamp: last block was {}, but received {}",
                last_record.block_context.timestamp,
                record.previous_block_timestamp
            );
            anyhow::ensure!(
                last_record.block_context.block_hashes.0[1..]
                    == record.block_context.block_hashes.0[..255],
                "inconsistent previous block hashes: last block's (#{}) was {:?}, but received new block's {:?}",
                last_record.block_context.block_number,
                last_record.block_context.block_hashes,
                record.block_context.block_hashes
            );
        }

        let expect_sl_chain_id_tx_after_upgrade = record
            .transactions
            .windows(2)
            .find(|window| {
                matches!(window[0].envelope(), ZkEnvelope::Upgrade(_))
                    && matches!(
                        window[1].as_system_tx_type(),
                        Some(SystemTxType::SetSLChainId(_, _))
                    )
            })
            .is_some();

        Ok(Some(PreparedBlockCommand {
            block_context: record.block_context,
            seal_policy: SealPolicy::UntilExhausted {
                allowed_to_finish_early: false,
            },
            invalid_tx_policy: InvalidTxPolicy::Abort,
            tx_source: MarkingTxStream::unmarkable(futures::stream::iter(record.transactions)),
            metrics_label: "replay",
            protocol_version: record.protocol_version,
            expected_block_output_hash: Some(record.block_output_hash),
            previous_block_timestamp: record.previous_block_timestamp,
            force_preimages: record.force_preimages,
            expect_sl_chain_id_tx_after_upgrade,
            starting_cursors: record.starting_cursors,
            interop_roots_per_block: self.config.interop_roots_per_block,
            strict_subpool_cleanup: false,
            direct_injection: false,
        }))
    }

    async fn rebuild(
        &mut self,
        rebuild: Box<RebuildCommand>,
    ) -> anyhow::Result<Option<PreparedBlockCommand<'_>>> {
        if self.last_block.is_none() {
            // As this is the first block we are rebuilding, we need to initialize the mempool.
            self.pool.init(&rebuild.replay_record).await;
        }

        let (previous_block_timestamp, next_cursors, block_hashes) =
            if let Some(last_block) = self.last_block.as_ref() {
                // We can't just use `rebuild`'s fields as the last block might have changed if we
                // are rebuilding a range of blocks
                (
                    last_block.record.block_context.timestamp,
                    last_block.next_cursors.clone(),
                    last_block
                        .record
                        .block_context
                        .block_hashes
                        .push(last_block.hash),
                )
            } else {
                (
                    rebuild.replay_record.previous_block_timestamp,
                    rebuild.replay_record.starting_cursors,
                    rebuild.replay_record.block_context.block_hashes,
                )
            };

        let block_number = rebuild.replay_record.block_context.block_number;
        let (execution_version, protocol_version) = (
            rebuild.replay_record.block_context.execution_version,
            rebuild.replay_record.protocol_version,
        );

        if rebuild.make_empty
            && rebuild
                .replay_record
                .transactions
                .iter()
                .any(|tx| matches!(tx.envelope(), ZkEnvelope::Upgrade(_)))
        {
            anyhow::bail!(
                "Cannot make an empty block when there is an upgrade transaction in the replay record for block {}",
                block_number
            );
        }

        let timestamp = if rebuild.reset_timestamp {
            (millis_since_epoch() / 1000) as u64
        } else {
            rebuild.replay_record.block_context.timestamp
        };
        let block_context = BlockContext {
            eip1559_basefee: rebuild.replay_record.block_context.eip1559_basefee,
            native_price: rebuild.replay_record.block_context.native_price,
            pubdata_price: rebuild.replay_record.block_context.pubdata_price,
            block_number,
            timestamp,
            blob_fee: rebuild.replay_record.block_context.blob_fee,
            chain_id: self.config.l2_chain_id,
            coinbase: self.config.fee_collector_address,
            block_hashes,
            gas_limit: self.config.gas_limit,
            pubdata_limit: self.config.pubdata_limit,
            // todo: initialize as source of randomness, i.e. the value of prevRandao
            mix_hash: Default::default(),
            execution_version,
        };
        let txs = if rebuild.make_empty {
            Vec::new()
        } else {
            let first_l1_tx = rebuild
                .replay_record
                .transactions
                .iter()
                .find(|tx| matches!(tx.envelope(), ZkEnvelope::L1(_)));
            // It's possible that we haven't processed some L1 transaction from previous blocks when rebuilding.
            // In that case we shouldn't consider next L1 txs when rebuilding.
            let filter_l1_txs =
                if let Some(ZkEnvelope::L1(l1_tx)) = first_l1_tx.map(|tx| tx.envelope()) {
                    l1_tx.priority_id() != next_cursors.l1_priority_id
                } else {
                    false
                };
            if filter_l1_txs {
                rebuild
                    .replay_record
                    .transactions
                    .into_iter()
                    .filter(|tx| !matches!(tx.envelope(), ZkEnvelope::L1(_)))
                    .collect()
            } else {
                rebuild.replay_record.transactions
            }
        };

        let expect_sl_chain_id_tx_after_upgrade = txs
            .windows(2)
            .find(|window| {
                matches!(window[0].envelope(), ZkEnvelope::Upgrade(_))
                    && matches!(
                        window[1].as_system_tx_type(),
                        Some(SystemTxType::SetSLChainId(_, _))
                    )
            })
            .is_some();

        Ok(Some(PreparedBlockCommand {
            expect_sl_chain_id_tx_after_upgrade,
            block_context,
            tx_source: MarkingTxStream::unmarkable(futures::stream::iter(txs)),
            seal_policy: SealPolicy::UntilExhausted {
                allowed_to_finish_early: true,
            },
            invalid_tx_policy: InvalidTxPolicy::RejectAndContinue {
                mark_in_source: false,
            },
            metrics_label: "rebuild",
            protocol_version,
            expected_block_output_hash: None,
            previous_block_timestamp,
            force_preimages: rebuild.replay_record.force_preimages,
            starting_cursors: next_cursors,
            interop_roots_per_block: self.config.interop_roots_per_block,
            strict_subpool_cleanup: false,
            direct_injection: false,
        }))
    }

    pub fn purge_transactions(&self, tx_hashes: Vec<TxHash>) {
        self.pool.purge_transactions(tx_hashes);
    }

    pub async fn on_canonical_state_change(
        &mut self,
        block_output: &BlockOutput,
        replay_record: &ReplayRecord,
        strict_subpool_cleanup: bool,
    ) {
        let mut next_cursors = replay_record.starting_cursors.clone();
        let outcome = self
            .pool
            .on_canonical_state_change(
                block_output.header.clone(),
                &block_output.account_diffs,
                replay_record,
                strict_subpool_cleanup,
            )
            .await;
        if let Some(last_l1_priority_id) = outcome.last_l1_priority_id {
            next_cursors.l1_priority_id = last_l1_priority_id + 1;
            EXECUTION_METRICS
                .next_l1_priority_id
                .set(next_cursors.l1_priority_id);
        }
        if let Some(last_interop_log_id) = outcome.last_interop_log_id {
            self.next_interop_tx_allowed_after = Instant::now() + self.config.service_block_delay;
            next_cursors.interop_root_id = last_interop_log_id + 1;
        }

        if let Some(last_migration_number) = outcome.last_migration_number {
            next_cursors.migration_number = last_migration_number + 1;
        }
        if let Some(target_sl_chain_id) = outcome.last_sl_chain_id_target {
            // Subsequent produced blocks will gate interop traffic on the new value (in particular:
            // stop including interop-root / interop-fee txs once we've migrated back to L1).
            // Otherwise, we will end up with blocks/batches that must be committed to L1 but
            // include interop txs which leads to `CommitBasedInteropNotSupported` revert.
            if self.current_sl_chain_id != target_sl_chain_id {
                tracing::info!(
                    previous_sl_chain_id = self.current_sl_chain_id,
                    new_sl_chain_id = target_sl_chain_id,
                    "applied SetSLChainId tx; updating runtime settlement layer pointer"
                );
                self.current_sl_chain_id = target_sl_chain_id;
            }
        }
        if let Some(last_interop_fee_number) = outcome.last_interop_fee_number {
            next_cursors.interop_fee_number = last_interop_fee_number + 1;
        }

        self.fee_provider.on_canonical_state_change(replay_record);
        self.last_block = Some(LastBlock {
            record: replay_record.clone(),
            hash: block_output.header.hash(),
            next_cursors,
        })
    }
}

pub fn millis_since_epoch() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Incorrect system time")
        .as_millis()
}
