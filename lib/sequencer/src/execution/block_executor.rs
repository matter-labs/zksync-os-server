use crate::execution::metrics::{EXECUTION_METRICS, SequencerState};
use crate::execution::utils::{BlockDump, hash_block_output};
use crate::execution::vm_wrapper::VmWrapper;
use crate::model::blocks::{InvalidTxPolicy, PreparedBlockCommand, SealPolicy};
use crate::model::debug_formatting::BlockOutputDebug;
use alloy::consensus::Transaction;
use alloy::primitives::TxHash;
use futures::StreamExt;
use std::pin::Pin;
use tokio::time::Sleep;
use vise::EncodeLabelValue;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_interface::types::{BlockContext, BlockOutput};
use zksync_os_metadata::NODE_SEMVER_VERSION;
use zksync_os_observability::ComponentStateHandle;
use zksync_os_storage_api::{
    MeteredViewState, OverriddenStateView, ReadStateHistory, ReplayRecord, WriteState,
};
use zksync_os_types::{SystemTxType, ZkTransaction, ZkTxType, ZksyncOsEncode};
// Note that this is a pure function without a container struct (e.g. `struct BlockExecutor`)
// MAINTAIN this to ensure the function is completely stateless - explicit or implicit.

// a side effect of this is that it's harder to pass config values (normally we'd just pass the whole config object)
// please be mindful when adding new parameters here

pub async fn execute_block<R: ReadStateHistory + WriteState>(
    mut command: PreparedBlockCommand<'_>,
    state: R,
    latency_tracker: &ComponentStateHandle<SequencerState>,
) -> Result<(BlockOutput, ReplayRecord, Vec<(TxHash, InvalidTransaction)>), BlockDump> {
    tracing::debug!(command = ?command, block_number=command.block_context.block_number, "Executing command");
    latency_tracker.enter_state(SequencerState::InitializingVm);
    let ctx = command.block_context;

    /* ---------- VM & state ----------------------------------------- */
    let state_view = state
        .state_view_at(ctx.block_number - 1)
        .map_err(|e| BlockDump {
            ctx,
            txs: Vec::new(),
            error: e.to_string(),
        })?;
    // Inject any forced preimages into the state view, these are expected to be added to the persistent state
    // after the block is executed.
    let state_view_with_force_preimages =
        OverriddenStateView::with_preimages(state_view, &command.force_preimages);
    let metered_state_view = MeteredViewState {
        component_state_tracker: latency_tracker.clone(),
        state_view: state_view_with_force_preimages,
    };
    let mut runner = VmWrapper::new(ctx, metered_state_view);

    let mut executed_txs = Vec::<ZkTransaction>::new();
    let mut cumulative_gas_used = 0u64;
    let mut purged_txs = Vec::new();

    let mut all_processed_txs = Vec::new();

    /* ---------- deadline config ------------------------------------ */
    let deadline_dur = match command.seal_policy {
        SealPolicy::Decide(d, _) => Some(d),
        SealPolicy::UntilExhausted { .. } => None,
    };
    let mut deadline: Option<Pin<Box<Sleep>>> = None; // will arm after 1st tx attempt
    let mut interop_roots_count = 0;

    /* ---------- main loop ------------------------------------------ */
    // seal_reason must only be used for observability - handling must remain generic
    let seal_reason = loop {
        latency_tracker.enter_state(SequencerState::WaitingForTx);
        tokio::select! {
            /* -------- deadline branch ------------------------------ */
            _ = async {
                    if let Some(d) = &mut deadline {
                        d.as_mut().await
                    }
                },
                if deadline.is_some()
            => {
                tracing::debug!(block_number = ctx.block_number,
                               txs = executed_txs.len(),
                               "deadline reached → sealing");
                break SealReason::Timeout;                                     // leave the loop ⇒ seal
            }

            /* -------- stream branch ------------------------------- */
            maybe_tx = command.tx_source.stream.next() => {
                latency_tracker.enter_state(SequencerState::Execution);
                let Some(tx) = maybe_tx else {
                    tracing::debug!(
                        block_number = ctx.block_number,
                        txs = executed_txs.len(),
                        "stream exhausted → sealing"
                    );
                    break SealReason::TxStreamExhausted;
                };

                if let Some(reason) = should_exclude_and_seal(&ctx, cumulative_gas_used, interop_roots_count, command.interop_roots_per_block, &tx) {
                    tracing::debug!(block_number = ctx.block_number, "sealing block as next tx cannot be included");
                    break reason;
                }

                tracing::debug!(
                    block_number=command.block_context.block_number,
                    tx_hash=?tx.hash(),
                    tx_index_in_block=executed_txs.len(),
                    cumulative_gas_used_before=cumulative_gas_used,
                    gas_limit=tx.inner.gas_limit(),
                    signer=?tx.inner.signer(),
                    "Executing transaction..."
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

                match runner.execute_next_tx(tx.clone().encode())
                    .await
                    .map_err(|e| {
                        BlockDump {
                            ctx,
                            txs: all_processed_txs.clone(),
                            error: e.to_string(),
                        }
                    })? {
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
                            "Transaction executed"
                        );

                        if let Some(SystemTxType::ImportInteropRoots(roots_count)) = tx.as_system_tx_type() {
                            interop_roots_count += roots_count;
                        }

                        let tx_type = tx.tx_type();
                        executed_txs.push(tx);
                        cumulative_gas_used += res.gas_used;
                        if tx_type == ZkTxType::Upgrade {
                            match &command.seal_policy {
                                SealPolicy::Decide(..) | SealPolicy::UntilExhausted { allowed_to_finish_early: true } => {
                                    tracing::debug!(block_number = ctx.block_number, "sealing block as upgrade tx was executed");
                                    break SealReason::UpgradeTx;
                                }
                                SealPolicy::UntilExhausted { allowed_to_finish_early: false } => {
                                    // We trust that the execution stream will not break protocol invariants.
                                    tracing::info!(block_number = ctx.block_number, "upgrade tx executed, but seal policy requires full exhaustion");
                                }
                            }
                        }

                        // If the only transaction provided is an SL chain id update transaction, we need to seal the block.
                        if let Some(SystemTxType::SetSLChainId) = executed_txs.last().unwrap().as_system_tx_type() {
                            match &command.seal_policy {
                                SealPolicy::Decide(..) | SealPolicy::UntilExhausted { allowed_to_finish_early: true } => {
                                    tracing::debug!(block_number = ctx.block_number, "sealing block as chain id update tx was executed");
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
                                tracing::debug!(block_number = ctx.block_number,
                                               txs = executed_txs.len(),
                                               "tx limit reached → sealing");
                                break SealReason::TxCountLimit
                            },
                            _ => {}
                        }
                    }
                    Err(e) => {
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
                            (ZkTxType::L2(_), InvalidTxPolicy::RejectAndContinue) => {
                                let rejection_method = rejection_method(&e);

                                // mark the tx as invalid regardless of the `rejection_method`.
                                command.tx_source.mark_last_l2_tx_as_invalid();
                                // add tx to `purged_txs` only if we are purging it.
                                match (rejection_method, command.seal_policy, executed_txs.is_empty()) {
                                    (TxRejectionMethod::Purge, _, _) => {
                                        purged_txs.push((*tx.hash(), e.clone()));
                                        tracing::info!(tx_hash = %tx.hash(), block_number = ctx.block_number, ?e, "invalid tx → purged");
                                    }
                                    (TxRejectionMethod::Skip, _, _) => {
                                        tracing::info!(tx_hash = %tx.hash(), block_number = ctx.block_number, ?e, "invalid tx → skipped");
                                    },
                                    // For Produce, don't seal if no transactions have been executed yet
                                    (TxRejectionMethod::SealBlock(reason), SealPolicy::Decide(..), true) => {
                                        purged_txs.push((*tx.hash(), e.clone()));
                                        tracing::info!(
                                            tx_hash = %tx.hash(),
                                            block_number = ctx.block_number,
                                            ?e,
                                            ?reason,
                                            "block limit reached on first tx for Produce → rejecting tx instead of sealing",
                                        );
                                    }
                                    (TxRejectionMethod::SealBlock(reason), _, _) => {
                                        tracing::debug!(tx_hash = %tx.hash(), block_number = ctx.block_number, ?e, ?reason, "sealing block by criterion");
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

    latency_tracker.enter_state(SequencerState::Sealing);

    /* ---------- seal & return ------------------------------------- */
    let mut output = runner.seal_block().await.map_err(|e| BlockDump {
        ctx,
        txs: all_processed_txs.clone(),
        error: e.context("seal_block()").to_string(),
    })?;

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

    tracing::info!(
        block_number = output.header.number,
        command = command.metrics_label,
        ?seal_reason,
        tx_count = executed_txs.len(),
        storage_writes = output.storage_writes.len(),
        preimages = output.published_preimages.len(),
        pubdata_bytes = output.pubdata.len(),
        cumulative_gas_used,
        purged_txs_len = purged_txs.len(),
        "Block sealed in block executor"
    );

    tracing::debug!(
        output = ?BlockOutputDebug(&output),
        block_number = output.header.number,
        "Block output"
    );

    let block_hash_output = hash_block_output(&output);

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
        output,
        ReplayRecord::new(
            ctx,
            command.starting_l1_priority_id,
            executed_txs,
            command.previous_block_timestamp,
            NODE_SEMVER_VERSION.clone(),
            command.protocol_version,
            block_hash_output,
            command.force_preimages,
            command.starting_interop_event_index,
        ),
        purged_txs,
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
        | InvalidTransaction::CallerGasLimitTooHigh => TxRejectionMethod::Purge,

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
