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
use zksync_os_interface::types::BlockOutput;
use zksync_os_observability::ComponentStateHandle;
use zksync_os_storage_api::{MeteredViewState, ReadStateHistory, ReplayRecord, WriteState};
use zksync_os_types::{ZkTransaction, ZkTxType, ZksyncOsEncode};
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
    let metered_state_view = MeteredViewState {
        component_state_tracker: latency_tracker.clone(),
        state_view,
    };
    let mut runner = VmWrapper::new(ctx, metered_state_view);

    let mut executed_txs = Vec::<ZkTransaction>::new();
    let mut cumulative_gas_used = 0u64;
    let mut purged_txs = Vec::new();

    let mut all_processed_txs = Vec::new();

    /* ---------- deadline config ------------------------------------ */
    let deadline_dur = match command.seal_policy {
        SealPolicy::Decide(d, _) => Some(d),
        SealPolicy::UntilExhausted => None,
    };
    let mut deadline: Option<Pin<Box<Sleep>>> = None; // will arm after 1st tx success

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
                tracing::debug!(block = ctx.block_number,
                               txs = executed_txs.len(),
                               "deadline reached → sealing");
                break SealReason::Timeout;                                     // leave the loop ⇒ seal
            }

            /* -------- stream branch ------------------------------- */
            maybe_tx = command.tx_source.next() => {
                latency_tracker.enter_state(SequencerState::Execution);
                match maybe_tx {
                    /* ----- got a transaction with gas limit within the block gas limit left --- */
                    Some(tx) if cumulative_gas_used + tx.inner.gas_limit() <= ctx.gas_limit => {

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

                                executed_txs.push(tx);
                                cumulative_gas_used += res.gas_used;

                                // arm the timer once, after the first successful tx
                                if deadline.is_none() && let Some(dur) = deadline_dur {
                                    deadline = Some(Box::pin(tokio::time::sleep(dur)));
                                }
                                match command.seal_policy {
                                    SealPolicy::Decide(_, limit) if executed_txs.len() >= limit => {
                                    tracing::debug!(block = ctx.block_number,
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
                                    (ZkTxType::L2(_), InvalidTxPolicy::RejectAndContinue) => {
                                        let rejection_method = rejection_method(&e);

                                        // mark the tx as invalid regardless of the `rejection_method`.
                                        command.tx_source.as_mut().mark_last_tx_as_invalid();
                                        // add tx to `purged_txs` only if we are purging it.
                                        match rejection_method {
                                            TxRejectionMethod::Purge => {
                                                purged_txs.push((*tx.hash(), e.clone()));
                                                tracing::warn!(tx_hash = %tx.hash(), block = ctx.block_number, ?e, "invalid tx → purged");
                                            }
                                            TxRejectionMethod::Skip => {
                                                tracing::warn!(tx_hash = %tx.hash(), block = ctx.block_number, ?e, "invalid tx → skipped");
                                            },
                                            TxRejectionMethod::SealBlock(reason) => {
                                                tracing::debug!(tx_hash = %tx.hash(), block = ctx.block_number, ?e, ?reason, "sealing block by criterion");
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
                    /* ----- got a transaction that cannot be included because of gas --- */
                    Some(tx) => {
                        if matches!(command.seal_policy, SealPolicy::UntilExhausted) {
                            let error = format!(
                                "tx {} cannot be included in block {}: cumulative gas {} exceeds block gas limit {}",
                                tx.hash(),
                                ctx.block_number,
                                cumulative_gas_used + tx.inner.gas_limit(),
                                ctx.gas_limit
                            );
                            let partial_seal_block_result = runner.seal_block().await;
                            tracing::error!(
                                ?partial_seal_block_result,
                                tx_hash = %tx.hash(),
                                block_number = ctx.block_number,
                                cumulative_gas_used = cumulative_gas_used + tx.inner.gas_limit(),
                                gas_limit = ctx.gas_limit,
                                "Transaction cannot be included in block: cumulative gas exceeds block gas limit"
                            );
                            return Err(
                                BlockDump {
                                    ctx,
                                    txs: all_processed_txs.clone(),
                                    error,
                                }
                            )
                        }

                        tracing::debug!(block = ctx.block_number, "sealing block as next tx cannot be included");
                        break SealReason::GasLimit
                    }
                    /* ----- tx stream exhausted  --------------------------- */
                    None => {
                        if executed_txs.is_empty() && matches!(command.seal_policy, SealPolicy::UntilExhausted)
                        {
                            // Replay path requires at least one tx.

                            // todo: maybe put this check to `ReplayRecord` instead - and just assert here? Or not even assert.
                            return Err(
                                BlockDump {
                                    ctx,
                                    txs: all_processed_txs.clone(),
                                    error: format!("empty replay for block {}", ctx.block_number),
                                }
                            )
                        }

                        tracing::debug!(
                            block = ctx.block_number,
                            txs = executed_txs.len(),
                            "stream exhausted → sealing"
                        );
                        break SealReason::Replay;
                    }
                }
            }
        }
    };
    latency_tracker.enter_state(SequencerState::Sealing);

    /* ---------- seal & return ------------------------------------- */
    let output = runner.seal_block().await.map_err(|e| BlockDump {
        ctx,
        txs: all_processed_txs.clone(),
        error: e.context("seal_block()").to_string(),
    })?;

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
        .observe(output.computaional_native_used);

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
            command.node_version,
            block_hash_output,
        ),
        purged_txs,
    ))
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
    Replay,
    Timeout,
    TxCountLimit,
    // Tx's gas limit + cumulative block gas > block gas limit - no execution attempt
    GasLimit,
    // VM returned `BlockGasLimitReached`
    GasVm,
    NativeCycles,
    Pubdata,
    L2ToL1Logs,
    Other,
}

fn rejection_method(error: &InvalidTransaction) -> TxRejectionMethod {
    match error {
        InvalidTransaction::InvalidEncoding
        | InvalidTransaction::InvalidStructure
        | InvalidTransaction::PriorityFeeGreaterThanMaxFee
        | InvalidTransaction::BaseFeeGreaterThanMaxFee
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
        | InvalidTransaction::GasPerPubdataTooHigh
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
        | InvalidTransaction::OtherUnrecoverable(_) => TxRejectionMethod::Purge,

        InvalidTransaction::GasPriceLessThanBasefee
        | InvalidTransaction::LackOfFundForMaxFee { .. }
        | InvalidTransaction::NonceTooHigh { .. } => TxRejectionMethod::Skip,

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
        InvalidTransaction::OtherLimitReached(_) => TxRejectionMethod::SealBlock(SealReason::Other),
    }
}
