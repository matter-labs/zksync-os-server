use crate::execution::metrics::{EXECUTION_METRICS, SequencerState};
use crate::execution::utils::{BlockDump, hash_block_output, parallel_producer_profile_enabled};
use crate::execution::vm_wrapper::VmWrapper;
use crate::model::blocks::{
    BlockOutputWithReads, InvalidTxPolicy, PreparedBlockCommand, SealPolicy,
};
use crate::model::debug_formatting::BlockOutputDebug;
use alloy::consensus::Transaction;
use alloy::primitives::TxHash;
use futures::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::time::Sleep;
use vise::EncodeLabelValue;
use zk_ee::memory::stack_trait::Stack;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::tracing::{AnyTracer, AnyTxValidator};
use zksync_os_metadata::NODE_SEMVER_VERSION;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_storage_api::{
    BlockContext, MeteredViewState, OverriddenStateView, ReplayRecord, ViewState,
};
use zksync_os_types::{SystemTxType, ZkTransaction, ZkTxType, ZksyncOsEncode};
// Note that this is a pure function without a container struct (e.g. `struct BlockExecutor`)
// MAINTAIN this to ensure the function is completely stateless - explicit or implicit.

// a side effect of this is that it's harder to pass config values (normally we'd just pass the whole config object)
// please be mindful when adding new parameters here

pub async fn execute_block_in_vm<V: ViewState>(
    mut command: PreparedBlockCommand<'_>,
    state_view: V,
    latency_tracker: &ComponentStateReporter,
    tracer: impl AnyTracer + Send + 'static,
    validator: impl AnyTxValidator + Send + 'static,
) -> Result<
    (
        BlockOutputWithReads,
        ReplayRecord,
        Vec<(TxHash, InvalidTransaction)>,
        bool,
    ),
    BlockDump,
> {
    tracing::info!(command = ?command, block_number=command.block_context.block_number, "Executing command");
    latency_tracker.enter_state(SequencerState::InitializingVm);
    let block_started_at = Instant::now();
    let ctx = command.block_context;

    /* ---------- VM & state ----------------------------------------- */
    // Inject any forced preimages into the state view, these are expected to be added to the persistent state
    // after the block is executed.
    let state_view_with_force_preimages =
        OverriddenStateView::with_preimages(state_view, &command.force_preimages);
    let metered_state_view = MeteredViewState::<SequencerState, _>::new(
        latency_tracker.clone(),
        state_view_with_force_preimages,
    );
    // Tracks how long the VM worker thread sits idle waiting for the next tx (the per-tx hand-off
    // ping-pong); reported in the per-block summary to gauge VM-thread utilization.
    let vm_idle_micros = Arc::new(AtomicU64::new(0));
    let mut runner = VmWrapper::new(
        ctx,
        metered_state_view,
        tracer,
        validator,
        vm_idle_micros.clone(),
    );
    let vm_init_elapsed = block_started_at.elapsed();

    let mut executed_txs = Vec::<ZkTransaction>::new();
    let mut cumulative_gas_used = 0u64;
    let mut purged_txs = Vec::new();

    // Per-block wall-clock accumulators (summed across all txs in this block), reported in the
    // per-block summary log when the block seals.
    let mut total_execute_tx_time = Duration::ZERO;
    let mut total_tx_stream_time = Duration::ZERO;
    // Direct-injection loop only: wall time spent inside `submit_tx` (the feeder→VM hand-off).
    let mut total_submit_time = Duration::ZERO;
    // Direct-injection loop only: wall time spent processing collected results (bookkeeping
    // between `next_result` awaits).
    let mut total_result_time = Duration::ZERO;

    let mut all_processed_txs = Vec::new();

    /* ---------- deadline config ------------------------------------ */
    let deadline_dur = match command.seal_policy {
        SealPolicy::Decide(d, _) => Some(d),
        SealPolicy::UntilExhausted { .. } => None,
    };
    // Armed on the 1st tx. The serial loop arms it once (absolute block deadline); the
    // direct-injection loop re-arms it per tx (idle linger — see there).
    let mut deadline: Option<Pin<Box<Sleep>>> = None;
    let mut interop_roots_count = 0;
    let expect_sl_chain_id_tx_after_upgrade = command.expect_sl_chain_id_tx_after_upgrade;

    if expect_sl_chain_id_tx_after_upgrade
        && let SealPolicy::Decide(duration, tx_limit) = command.seal_policy
        && tx_limit < 2
    {
        command.seal_policy = SealPolicy::Decide(duration, 2);
        tracing::warn!(
            "Upgrade v31 requires two txs (Upgrade and SetSLChainId) to be included in the first v31 block. \
                `max_transactions_in_block` is ignored"
        );
    }

    /* ---------- main loop ------------------------------------------ */
    // seal_reason must only be used for observability - handling must remain generic
    let seal_reason = if command.direct_injection {
        // ===== Direct-injection bench path =====
        // Keep up to `VM_PIPELINE_DEPTH` transactions in flight so the VM worker runs them
        // back-to-back instead of stalling on the per-tx hand-off (the dominant cost at high TPS).
        // Valid only because direct injection feeds plain L2 transfers (no upgrade/SL-chain-id/
        // system txs) with huge gas and pubdata limits, so the only seal reasons are the tx-count
        // limit and the deadline. Set via `PreparedBlockCommand::direct_injection`.
        use std::collections::VecDeque;
        // Sized above the bench's max transactions-per-block so the feeder submits the whole block
        // before draining: the collector isn't parked while feeding (no wakes) and the result
        // buffer is full while draining (no parks), removing the per-tx cross-thread park/unpark.
        // Must be <= `VM_CHANNEL_CAPACITY` (the channels backpressure at that bound).
        const DEFAULT_VM_PIPELINE_DEPTH: usize = 30_000;
        let vm_pipeline_depth = std::env::var("DIRECT_INJECTION_VM_PIPELINE_DEPTH")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|depth| *depth > 0)
            .map(|depth| depth.min(DEFAULT_VM_PIPELINE_DEPTH))
            .unwrap_or(DEFAULT_VM_PIPELINE_DEPTH);
        let tx_limit = match command.seal_policy {
            SealPolicy::Decide(_, limit) => limit,
            SealPolicy::UntilExhausted { .. } => usize::MAX,
        };
        let mut pending: VecDeque<ZkTransaction> = VecDeque::new();
        let mut pending_seal: Option<SealReason> = None;
        // The seal deadline is an idle linger, not an absolute cap: parallel lane blocks pull
        // LIVE from their lane channel, so filling up to the tx-count limit takes a
        // feed-rate-dependent stretch of time; an absolute deadline would seal them half-full
        // and double the per-block overhead. The block keeps accumulating while the feed keeps
        // up and seals `deadline_dur` after it goes quiet (or earlier, on the tx-count limit).
        // Implemented lazily: per tx we only stamp `last_tx_at` — the sleep is re-armed to
        // `last_tx_at + dur` when it fires, ONE timer op per linger period. Re-arming per tx
        // (`Sleep::reset`) hammers the tokio timer lock at the aggregate tx rate and convoys
        // the feed loop on high-CCD-count machines.
        let mut last_tx_at = tokio::time::Instant::now();
        // Per-tx metric updates are batched into these locals and flushed once per block after
        // the loop: K concurrent feeders doing ~6 atomic RMWs per tx on the same shared metric
        // cachelines collapse on many-CCD machines (~150µs/tx of cacheline ping-pong observed on
        // a 96-core 7995WX — it capped the whole bench while the VM sat idle). The per-tx
        // histograms (gas/native/pubdata per tx) are skipped here; the per-block histograms
        // flushed at seal still cover the bench path.
        let mut status_success_txs = 0u64;
        let mut status_failure_txs = 0u64;
        let seal = loop {
            // Feed up to the pipeline depth, unless we've decided to seal or would exceed the
            // tx-count limit. `submit_tx` backpressures on the (depth-sized) channel, but the
            // `pending.len() < vm_pipeline_depth` guard keeps it from ever blocking here.
            while pending_seal.is_none()
                && pending.len() < vm_pipeline_depth
                && executed_txs.len() + pending.len() < tx_limit
            {
                let tx_wait_start = Instant::now();
                let maybe_tx = tokio::select! {
                    _ = async {
                            if let Some(d) = &mut deadline {
                                d.as_mut().await
                            }
                        },
                        if deadline.is_some()
                    => {
                        let dur = deadline_dur.expect("armed deadline implies a duration");
                        let idle_deadline = last_tx_at + dur;
                        if tokio::time::Instant::now() >= idle_deadline {
                            pending_seal = Some(SealReason::Timeout);
                            None
                        } else {
                            // Txs kept flowing since the sleep was armed: push it out to
                            // `last_tx_at + dur` and keep feeding.
                            deadline
                                .as_mut()
                                .expect("deadline branch requires an armed sleep")
                                .as_mut()
                                .reset(idle_deadline);
                            continue;
                        }
                    }
                    maybe_tx = command.tx_source.stream.next() => {
                        total_tx_stream_time += tx_wait_start.elapsed();
                        maybe_tx
                    }
                };
                if pending_seal.is_some() {
                    break;
                }
                let Some(tx) = maybe_tx else {
                    pending_seal = Some(SealReason::TxStreamExhausted);
                    break;
                };
                last_tx_at = tokio::time::Instant::now();
                if deadline.is_none()
                    && let Some(dur) = deadline_dur
                {
                    deadline = Some(Box::pin(tokio::time::sleep(dur)));
                }
                all_processed_txs.push(tx.clone());
                let submit_start = Instant::now();
                runner
                    .submit_tx(tx.clone().encode())
                    .await
                    .map_err(|e| BlockDump {
                        ctx,
                        txs: all_processed_txs.clone(),
                        error: e.to_string(),
                    })?;
                total_submit_time += submit_start.elapsed();
                pending.push_back(tx);
            }
            if pending_seal.is_none() && executed_txs.len() + pending.len() >= tx_limit {
                pending_seal = Some(SealReason::TxCountLimit);
            }

            // Collect one result; results arrive in submission (FIFO) order.
            let Some(tx) = pending.pop_front() else {
                tracing::info!(
                    block_number = ctx.block_number,
                    txs = executed_txs.len(),
                    "direct-injection block sealing: {:?}",
                    pending_seal
                );
                break pending_seal.unwrap_or(SealReason::TxStreamExhausted);
            };
            let exec_start = Instant::now();
            let exec_result = runner.next_result().await.map_err(|e| BlockDump {
                ctx,
                txs: all_processed_txs.clone(),
                error: e.to_string(),
            })?;
            total_execute_tx_time += exec_start.elapsed();
            let result_start = Instant::now();
            match exec_result {
                Ok(res) => {
                    // Batched — see `status_success_txs` above.
                    if res.status {
                        status_success_txs += 1;
                    } else {
                        status_failure_txs += 1;
                    }
                    cumulative_gas_used += res.gas_used;
                    executed_txs.push(tx);
                }
                Err(e) => {
                    if let ZkTxType::L2(_) = tx.tx_type() {
                        // Direct injection feeds valid L2 transfers; treat any rejection as a purge.
                        purged_txs.push((*tx.hash(), e.clone()));
                        tracing::info!(
                            block_number = ctx.block_number,
                            "Invalid L2 tx {} purged in direct-injection block {}: error={e:?}, nonce={:?}",
                            tx.hash(),
                            ctx.block_number,
                            tx.nonce(),
                        );
                    } else {
                        return Err(BlockDump {
                            ctx,
                            txs: all_processed_txs.clone(),
                            error: format!(
                                "unexpected non-L2 tx ({:?}) in direct-injection block: {e:?} ({})",
                                tx.tx_type(),
                                tx.hash()
                            ),
                        });
                    }
                }
            }
            total_result_time += result_start.elapsed();
        };
        EXECUTION_METRICS
            .executed_transactions
            .inc_by(status_success_txs + status_failure_txs);
        EXECUTION_METRICS.transaction_status[&"success"].inc_by(status_success_txs);
        EXECUTION_METRICS.transaction_status[&"failure"].inc_by(status_failure_txs);
        seal
    } else {
        loop {
            latency_tracker.enter_state(SequencerState::WaitingForTx);
            let tx_wait_start = Instant::now();
            tokio::select! {
                /* -------- deadline branch ------------------------------ */
                _ = async {
                        if let Some(d) = &mut deadline {
                            d.as_mut().await
                        }
                    },
                    if deadline.is_some()
                => {
                    tracing::info!(block_number = ctx.block_number,
                                   txs = executed_txs.len(),
                                   "deadline reached → sealing");
                    break SealReason::Timeout;                                     // leave the loop ⇒ seal
                }

                /* -------- stream branch ------------------------------- */
                maybe_tx = command.tx_source.stream.next() => {
                    total_tx_stream_time += tx_wait_start.elapsed();
                    let Some(tx) = maybe_tx else {
                        tracing::info!(
                            block_number = ctx.block_number,
                            txs = executed_txs.len(),
                            "stream exhausted → sealing"
                        );
                        break SealReason::TxStreamExhausted;
                    };

                    if let Some(reason) = should_exclude_and_seal(&ctx, cumulative_gas_used, interop_roots_count, command.interop_roots_per_block, &tx) {
                        tracing::info!(block_number = ctx.block_number, "sealing block as next tx cannot be included");
                        break reason;
                    }

                    tracing::debug!(
                        block_number=command.block_context.block_number,
                        "Executing transaction {:?} ({:?}) in block {} at index {} signer {:?} nonce {} with gas limit {} and cumulative gas used {cumulative_gas_used}...",
                        tx.hash(),
                        tx.tx_type(),
                        command.block_context.block_number,
                        executed_txs.len(),
                        tx.inner.signer(),
                        tx.nonce(),
                        tx.inner.gas_limit()
                    );

                    all_processed_txs.push(tx.clone());

                    // Arm the deadline on the first tx attempt (success or failure).
                    // This prevents indefinite hangs when all L2 txs fail validation
                    // (e.g. BaseFeeGreaterThanMaxFee) and no L1 txs arrive to break
                    // the deadlock. Without this, the block executor would wait forever
                    // because the deadline only armed on success, and the sender is
                    // marked invalid in the BestTransactions iterator after a failure.
                    // Note that this behavior may result in an empty block being mined,
                    // which is supported server behavour.
                    if deadline.is_none() && let Some(dur) = deadline_dur {
                        deadline = Some(Box::pin(tokio::time::sleep(dur)));
                    }

                    let exec_start = Instant::now();
                    let exec_result = runner
                        .execute_next_tx(tx.clone().encode())
                        .await
                        .map_err(|e| {
                            BlockDump {
                                ctx,
                                txs: all_processed_txs.clone(),
                                error: e.to_string(),
                            }
                        })?;
                    total_execute_tx_time += exec_start.elapsed();
                    match exec_result {
                        Ok(res) => {
                            EXECUTION_METRICS.executed_transactions.inc();
                            EXECUTION_METRICS.transaction_gas_used.observe(res.gas_used);
                            EXECUTION_METRICS.transaction_native_used.observe(res.native_used);
                            EXECUTION_METRICS.transaction_computation_native_used.observe(res.computational_native_used);
                            EXECUTION_METRICS.transaction_pubdata_used.observe(res.pubdata_used);
                            let status_str = if res.status  {"success"} else {"failure"};
                            EXECUTION_METRICS.transaction_status[&status_str].inc();
                            tracing::debug!(
                                block_number=command.block_context.block_number,
                                output=?res,
                                "Transaction {:?} executed with status {status_str} in block {}",
                                tx.hash(),
                                command.block_context.block_number
                            );

                            if let Some(SystemTxType::ImportInteropRoots(roots_count)) = tx.as_system_tx_type() {
                                interop_roots_count += roots_count;
                            }

                            let tx_type = tx.tx_type();
                            executed_txs.push(tx);
                            cumulative_gas_used += res.gas_used;
                            if tx_type == ZkTxType::Upgrade {
                                if !res.status {
                                    let tx_hash = executed_txs.last().unwrap().hash();
                                    tracing::error!(
                                        block_number = ctx.block_number,
                                        ?tx_hash,
                                        revert_output = ?res.output,
                                        "Upgrade transaction reverted"
                                    );
                                    return Err(BlockDump {
                                        ctx,
                                        txs: all_processed_txs.clone(),
                                        error: format!("upgrade tx {tx_hash} reverted"),
                                    });
                                }
                                if expect_sl_chain_id_tx_after_upgrade {
                                    tracing::info!(
                                        block_number = ctx.block_number,
                                        "upgrade tx executed, continuing with the sequencer-injected SL chain id tx"
                                    );
                                } else {
                                    match &command.seal_policy {
                                        SealPolicy::Decide(..) | SealPolicy::UntilExhausted { allowed_to_finish_early: true } => {
                                            tracing::info!(block_number = ctx.block_number, "sealing block as upgrade tx was executed");
                                            break SealReason::UpgradeTx;
                                        }
                                        SealPolicy::UntilExhausted { allowed_to_finish_early: false } => {
                                            // We trust that the execution stream will not break protocol invariants.
                                            tracing::info!(block_number = ctx.block_number, "upgrade tx executed, but seal policy requires full exhaustion");
                                        }
                                    }
                                }
                            }

                            // If the transaction provided is an SL chain id update transaction, we need to seal the block.
                            if let Some(SystemTxType::SetSLChainId(_, _)) = executed_txs.last().unwrap().as_system_tx_type() {
                                match &command.seal_policy {
                                    SealPolicy::Decide(..) | SealPolicy::UntilExhausted { allowed_to_finish_early: true } => {
                                        tracing::info!(block_number = ctx.block_number, "sealing block as chain id update tx was executed");
                                        break SealReason::SLChainIdUpdateTx;
                                    }
                                    SealPolicy::UntilExhausted { allowed_to_finish_early: false } => {
                                        // We trust that the execution stream will not break protocol invariants.
                                        tracing::info!(block_number = ctx.block_number, "chain id update tx executed, but seal policy requires full exhaustion");
                                    }
                                }
                            }

                            match command.seal_policy {
                                SealPolicy::Decide(_, limit) if executed_txs.len() >= limit => {
                                    tracing::info!(block_number = ctx.block_number,
                                                   txs = executed_txs.len(),
                                                   "tx limit reached → sealing");
                                    break SealReason::TxCountLimit
                                },
                                _ => {}
                            }
                        }
                        Err(e) => {
                            tracing::info!(
                                block_number = command.block_context.block_number,
                                "Transaction {:?} ({}) in block {} failed: {e:?}",
                                tx.tx_type(),
                                tx.hash(),
                                command.block_context.block_number
                            );

                            match (tx.tx_type(), command.invalid_tx_policy) {
                                (ZkTxType::L1 | ZkTxType::Upgrade, _) => {
                                    return Err(
                                        BlockDump {
                                            ctx,
                                            txs: all_processed_txs.clone(),
                                            error: format!("invalid {} tx: {e:?} ({})", tx.tx_type(), tx.hash()),
                                        }
                                    )
                                }
                                (ZkTxType::System, _) => {
                                    return Err(
                                        BlockDump {
                                            ctx,
                                            txs: all_processed_txs.clone(),
                                            error: format!("invalid system tx with type {:?}: {e:?} ({})", tx.as_system_tx_type(), tx.hash()),
                                        }
                                    )
                                }
                                (
                                    ZkTxType::L2(_),
                                    InvalidTxPolicy::RejectAndContinue { mark_in_source },
                                ) => {
                                    let rejection_method = rejection_method(&e);
                                    if mark_in_source {
                                        command.tx_source.mark_last_l2_tx_as_invalid();
                                    }

                                    match (rejection_method, command.seal_policy, executed_txs.is_empty()) {
                                        (TxRejectionMethod::Purge, _, _) => {
                                            purged_txs.push((*tx.hash(), e.clone()));
                                            tracing::info!(
                                                block_number = ctx.block_number,
                                                "Invalid L2 tx {} was purged in block {}: error={e:?}, source_marked_invalid={}, nonce={:?}",
                                                tx.hash(),
                                                ctx.block_number,
                                                mark_in_source,
                                                tx.nonce(),
                                            );
                                        }
                                        (TxRejectionMethod::Skip, _, _) => {
                                            tracing::info!(
                                                block_number = ctx.block_number,
                                                "Invalid L2 tx {} was skipped in block {}: error={e:?}, source_marked_invalid={}, nonce={:?}",
                                                tx.hash(),
                                                ctx.block_number,
                                                mark_in_source,
                                                tx.nonce(),
                                            );
                                        },
                                        // For Produce, don't seal if no transactions have been executed yet
                                        (TxRejectionMethod::SealBlock(reason), SealPolicy::Decide(..), true) => {
                                            purged_txs.push((*tx.hash(), e.clone()));
                                            tracing::info!(
                                                block_number = ctx.block_number,
                                                "Block {} hit a sealing criterion while processing first L2 tx {}: reason={reason:?}, error={e:?}, source_marked_invalid={}, nonce={:?}; rejecting tx instead of sealing",
                                                ctx.block_number,
                                                tx.hash(),
                                                mark_in_source,
                                                tx.nonce(),
                                            );
                                        }
                                        (TxRejectionMethod::SealBlock(reason), _, _) => {
                                            tracing::info!(
                                                block_number = ctx.block_number,
                                                "Sealing block {} before L2 tx {} because it hit a sealing criterion: reason={reason:?}, error={e:?}, nonce={:?}",
                                                ctx.block_number,
                                                tx.hash(),
                                                tx.nonce(),
                                            );
                                            break reason;
                                        }
                                    }
                                }
                                (ZkTxType::L2(_), InvalidTxPolicy::Abort) => {
                                    return Err(
                                        BlockDump {
                                            ctx,
                                            txs: all_processed_txs.clone(),
                                            error: format!("invalid l2 tx: {e:?} ({})", tx.hash()),
                                        }
                                    )
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    // seal reason validation
    match command.seal_policy {
        SealPolicy::Decide(_, _) => {
            if seal_reason == SealReason::TxStreamExhausted {
                return Err(BlockDump {
                    ctx,
                    txs: all_processed_txs.clone(),
                    error: format!("tx stream was unexpectedly exhausted {}", ctx.block_number),
                });
            }
        }
        SealPolicy::UntilExhausted {
            allowed_to_finish_early,
        } => {
            if !allowed_to_finish_early && seal_reason != SealReason::TxStreamExhausted {
                return Err(BlockDump {
                    ctx,
                    txs: all_processed_txs.clone(),
                    error: format!(
                        "block was expected to be sealed due to stream exhaustion, but sealed due to {:?} instead, block {}",
                        seal_reason, ctx.block_number
                    ),
                });
            }
        }
    }

    /* ---------- seal & return ------------------------------------- */
    let seal_started_at = Instant::now();
    let mut output_with_reads = runner.seal_block().await.map_err(|e| BlockDump {
        ctx,
        txs: all_processed_txs.clone(),
        error: e.context("seal_block()").to_string(),
    })?;
    let seal_elapsed = seal_started_at.elapsed();
    let unique_reads_count = output_with_reads.read_keys().len();
    let total_read_time = output_with_reads.total_read_time();
    let read_count = output_with_reads.read_count();
    let output = output_with_reads.inner_mut();

    // Since we've overridden the state, we need to insert any forced preimages into the output as well.
    // Note: the fact that we're doing it here, would also affect the block output hash,
    // so we'll be able to check consistency upon re-execution.
    output
        .published_preimages
        .extend(command.force_preimages.iter().map(|(k, v)| (*k, v.clone())));

    // Remove failed transactions from output.tx_results.
    // Note: Rejected transactions don't affect the VM state or output,
    // yet they are still returned in output.tx_results.
    // This results in an inconsistency - transaction exists in output, but doesn't exist in
    // replay_record.transactions.
    // Here, we manually remove all such tx_results from VM output.
    output.tx_results.retain(|tx| tx.is_ok());

    EXECUTION_METRICS
        .storage_writes_per_block
        .observe(output.storage_writes.len() as u64);
    EXECUTION_METRICS.seal_reason[&seal_reason].inc();
    EXECUTION_METRICS.gas_per_block.observe(cumulative_gas_used);
    EXECUTION_METRICS
        .pubdata_per_block
        .observe(output.pubdata.len() as u64);
    EXECUTION_METRICS
        .transactions_per_block
        .observe(executed_txs.len() as u64);
    EXECUTION_METRICS
        .computational_native_used_per_block
        .observe(output.computational_native_used);

    let block_hash_output = hash_block_output(output);

    // How long the VM worker thread spent blocked waiting for the next tx. If this is a large
    // fraction of `execute_tx_time`, the VM is idle between txs (hand-off ping-pong, not CPU-bound).
    let vm_idle_time = Duration::from_micros(vm_idle_micros.load(Ordering::Relaxed));

    tracing::info!(
        block_number = output.header.number,
        "Block {block_number} ({label}) sealed because of {seal_reason:?} in block executor \
        with {tx_count} transactions ({purged_tx_count} purged) and {cumulative_gas_used} gas. \
        Block hash output: {block_hash_output:?}, canonical hash: {canonical_hash:?}. \
        storage writes: {write_count}, unique reads: {unique_reads_count}, preimages: {preimages_count}, pubdata bytes: {pubdata_len}. \
        execute_tx_time: {total_execute_tx_time:?}, tx_stream_time: {total_tx_stream_time:?}, \
        read_time: {total_read_time:?} over {read_count} reads, vm_idle_time: {vm_idle_time:?}.",
        block_number = output.header.number,
        label = command.metrics_label,
        tx_count = executed_txs.len(),
        purged_tx_count = purged_txs.len(),
        canonical_hash = output.header.hash(),
        write_count = output.storage_writes.len(),
        preimages_count = output.published_preimages.len(),
        pubdata_len = output.pubdata.len(),
    );

    // Bench-only per-block VM profile (ERROR level so it survives `RUST_LOG=warn` runs): splits
    // the executor's per-round `exec_elapsed` into feed wait vs VM work. Reading it:
    // `tx_stream_time` dominating (with high `vm_idle_time`) → the block is starved by its lane
    // feed; `execute_tx_time` dominating (with low `vm_idle_time`) → genuinely VM-bound;
    // `read_time` → the share of VM time blocked on server-side state reads.
    if parallel_producer_profile_enabled() {
        tracing::error!(
            block_number = output.header.number,
            label = command.metrics_label,
            txs = executed_txs.len(),
            ?seal_reason,
            block_elapsed = ?block_started_at.elapsed(),
            ?vm_init_elapsed,
            tx_stream_time = ?total_tx_stream_time,
            submit_time = ?total_submit_time,
            execute_tx_time = ?total_execute_tx_time,
            result_time = ?total_result_time,
            vm_idle_time = ?vm_idle_time,
            read_time = ?total_read_time,
            read_count,
            ?seal_elapsed,
            "block vm profile"
        );
    }

    tracing::debug!(
        output = ?BlockOutputDebug(output),
        block_number = output.header.number,
        "Full block {} output",
        output.header.number,
    );

    // Check if the block output matches the expected hash.
    if let Some(expected_hash) = command.expected_block_output_hash
        && expected_hash != block_hash_output
    {
        let error = format!(
            "Block #{} output hash mismatch: expected {expected_hash}, got {block_hash_output}",
            ctx.block_number,
        );
        tracing::error!(?output, block_number = ctx.block_number, expected = %expected_hash, actual = %block_hash_output, "Block output hash mismatch");
        return Err(BlockDump {
            ctx,
            txs: all_processed_txs.clone(),
            error,
        });
    }

    Ok((
        output_with_reads,
        ReplayRecord::new(
            ctx,
            executed_txs,
            command.previous_block_timestamp,
            NODE_SEMVER_VERSION.clone(),
            command.protocol_version,
            block_hash_output,
            command.force_preimages,
            command.starting_cursors,
        ),
        purged_txs,
        command.strict_subpool_cleanup,
    ))
}

fn should_exclude_and_seal(
    ctx: &BlockContext,
    cumulative_gas_used: u64,
    interop_roots_count: u64,
    interop_roots_per_block: u64,
    tx: &ZkTransaction,
) -> Option<SealReason> {
    if cumulative_gas_used + tx.inner.gas_limit() > ctx.gas_limit {
        return Some(SealReason::GasLimit);
    }
    if let Some(SystemTxType::ImportInteropRoots(roots_count)) = tx.as_system_tx_type()
        && interop_roots_count + roots_count > interop_roots_per_block
    {
        return Some(SealReason::LimitedInteropOnlyBlock);
    }
    None
}

enum TxRejectionMethod {
    // purge tx from the mempool
    Purge,
    // skip tx and all its descendants for the current block
    Skip,
    // block is out of some resource, so it should be sealed.
    SealBlock(SealReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EncodeLabelValue)]
#[metrics(label = "seal_reason", rename_all = "snake_case")]
pub enum SealReason {
    TxStreamExhausted,
    Timeout,
    TxCountLimit,
    // Tx's gas limit + cumulative block gas > block gas limit - no execution attempt
    GasLimit,
    // VM returned `BlockGasLimitReached`
    GasVm,
    NativeCycles,
    Pubdata,
    L2ToL1Logs,
    Blobs,
    // We executed upgrade transaction
    UpgradeTx,
    // We executed SL chain id update transaction
    SLChainIdUpdateTx,
    // Block contains only interop transactions with a limit of interop roots per block reached
    LimitedInteropOnlyBlock,
    Other,
}

fn rejection_method(error: &InvalidTransaction) -> TxRejectionMethod {
    match error {
        InvalidTransaction::InvalidEncoding
        | InvalidTransaction::InvalidStructure
        | InvalidTransaction::PriorityFeeGreaterThanMaxFee
        | InvalidTransaction::CallerGasLimitMoreThanBlock
        | InvalidTransaction::CallerGasLimitMoreThanTxLimit
        | InvalidTransaction::CallGasCostMoreThanGasLimit
        | InvalidTransaction::RejectCallerWithCode
        | InvalidTransaction::OverflowPaymentInTransaction
        | InvalidTransaction::NonceOverflowInTransaction
        | InvalidTransaction::NonceTooLow { .. }
        | InvalidTransaction::MalleableSignature
        | InvalidTransaction::IncorrectFrom { .. }
        | InvalidTransaction::CreateInitCodeSizeLimit
        | InvalidTransaction::InvalidChainId
        | InvalidTransaction::AccessListNotSupported
        | InvalidTransaction::PubdataPriceTooHigh
        | InvalidTransaction::BlockGasLimitTooHigh
        | InvalidTransaction::UpgradeTxNotFirst
        | InvalidTransaction::Revert { .. }
        | InvalidTransaction::ReceivedInsufficientFees { .. }
        | InvalidTransaction::InvalidMagic
        | InvalidTransaction::InvalidReturndataLength
        | InvalidTransaction::OutOfGasDuringValidation
        | InvalidTransaction::OutOfNativeResourcesDuringValidation
        | InvalidTransaction::NonceUsedAlready
        | InvalidTransaction::NonceNotIncreased
        | InvalidTransaction::PaymasterReturnDataTooShort
        | InvalidTransaction::PaymasterInvalidMagic
        | InvalidTransaction::PaymasterContextInvalid
        | InvalidTransaction::PaymasterContextOffsetTooLong
        | InvalidTransaction::AuthListIsEmpty
        | InvalidTransaction::BlobElementIsNotSupported
        | InvalidTransaction::EIP7623IntrinsicGasIsTooLow
        | InvalidTransaction::NativeResourcesAreTooExpensive
        | InvalidTransaction::OtherUnrecoverable(_)
        | InvalidTransaction::EIP7702HasNullDestination
        | InvalidTransaction::BlobListTooLong
        | InvalidTransaction::EmptyBlobList
        | InvalidTransaction::FilteredByValidator
        | InvalidTransaction::CallerGasLimitTooHigh
        | InvalidTransaction::FriProofTxNotSupported
        | InvalidTransaction::FriProofSidecarMissing
        | InvalidTransaction::FriProofVerificationFailed
        | InvalidTransaction::FriProofStatementHashMismatch
        | InvalidTransaction::TooManyFriStatements => TxRejectionMethod::Purge,

        InvalidTransaction::GasPriceLessThanBasefee
        | InvalidTransaction::LackOfFundForMaxFee { .. }
        | InvalidTransaction::NonceTooHigh { .. }
        | InvalidTransaction::BaseFeeGreaterThanMaxFee
        | InvalidTransaction::BlobBaseFeeGreaterThanMaxFeePerBlobGas => TxRejectionMethod::Skip,

        InvalidTransaction::BlockGasLimitReached => TxRejectionMethod::SealBlock(SealReason::GasVm),
        InvalidTransaction::BlockNativeLimitReached => {
            TxRejectionMethod::SealBlock(SealReason::NativeCycles)
        }
        InvalidTransaction::BlockPubdataLimitReached => {
            TxRejectionMethod::SealBlock(SealReason::Pubdata)
        }
        InvalidTransaction::BlockL2ToL1LogsLimitReached => {
            TxRejectionMethod::SealBlock(SealReason::L2ToL1Logs)
        }
        InvalidTransaction::BlockBlobGasLimitReached => {
            TxRejectionMethod::SealBlock(SealReason::Blobs)
        }
        InvalidTransaction::OtherLimitReached(_) => TxRejectionMethod::SealBlock(SealReason::Other),
    }
}
