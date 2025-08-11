use crate::execution::metrics::EXECUTION_METRICS;
use crate::execution::utils::hash_block_output;
use crate::execution::vm_wrapper::VmWrapper;
use crate::metrics::GENERAL_METRICS;
use crate::model::blocks::{InvalidTxPolicy, PreparedBlockCommand, SealPolicy};
use alloy::consensus::Transaction;
use alloy::primitives::TxHash;
use anyhow::{Result, anyhow};
use futures::StreamExt;
use std::pin::Pin;
use tokio::time::Sleep;
use zk_os_forward_system::run::{BlockOutput, InvalidTransaction};
use zksync_os_state::StateHandle;
use zksync_os_storage_api::ReplayRecord;
use zksync_os_types::{ZkTransaction, ZkTxType, ZksyncOsEncode};
// Note that this is a pure function without a container struct (e.g. `struct BlockExecutor`)
// MAINTAIN this to ensure the function is completely stateless - explicit or implicit.

// a side effect of this is that it's harder to pass config values (normally we'd just pass the whole config object)
// please be mindful when adding new parameters here

pub async fn execute_block(
    mut command: PreparedBlockCommand<'_>,
    state: StateHandle,
) -> Result<(BlockOutput, ReplayRecord, Vec<(TxHash, InvalidTransaction)>)> {
    let ctx = command.block_context;

    /* ---------- VM & state ----------------------------------------- */
    let state_view = state.state_view_at_block(ctx.block_number - 1)?;
    let mut runner = VmWrapper::new(ctx, state_view);

    let mut executed_txs = Vec::<ZkTransaction>::new();
    let mut cumulative_gas_used = 0u64;
    let mut purged_txs = Vec::new();

    let tx_can_be_included = |tx: &ZkTransaction, cumulative_gas_used: u64| -> bool {
        cumulative_gas_used + tx.inner.gas_limit() <= ctx.gas_limit
    };

    /* ---------- deadline config ------------------------------------ */
    let deadline_dur = match command.seal_policy {
        SealPolicy::Decide(d, _) => Some(d),
        SealPolicy::UntilExhausted => None,
    };
    let mut deadline: Option<Pin<Box<Sleep>>> = None; // will arm after 1st tx success

    /* ---------- main loop ------------------------------------------ */
    loop {
        let wait_for_tx_latency_observer =
            EXECUTION_METRICS.block_execution_stages[&"wait_for_tx"].start();
        tokio::select! {
            /* -------- deadline branch ------------------------------ */
            _ = async {
                    if let Some(d) = &mut deadline {
                        d.as_mut().await
                    }
                },
                if deadline.is_some()
            => {
                tracing::info!(block = ctx.block_number,
                               txs = executed_txs.len(),
                               "deadline reached → sealing");
                break;                                     // leave the loop ⇒ seal
            }

            /* -------- stream branch ------------------------------- */
            maybe_tx = command.tx_source.next() => {
                match maybe_tx {
                    /* ----- got a transaction ---------------------- */
                    Some(tx) if tx_can_be_included(&tx, cumulative_gas_used) => {
                        tracing::info!("Executing tx: {:?}", tx.hash());
                        wait_for_tx_latency_observer.observe();
                        let execute_latency_observer = EXECUTION_METRICS.block_execution_stages[&"execute"].start();
                        match runner.execute_next_tx(tx.clone().encode()).await {
                            Ok(res) => {
                                execute_latency_observer.observe();
                                GENERAL_METRICS.executed_transactions[&command.metrics_label].inc();

                                executed_txs.push(tx);
                                cumulative_gas_used += res.gas_used;

                                // arm the timer once, after the first successful tx
                                if deadline.is_none() && let Some(dur) = deadline_dur {
                                    deadline = Some(Box::pin(tokio::time::sleep(dur)));
                                }
                                match command.seal_policy {
                                    SealPolicy::Decide(_, limit) if executed_txs.len() >= limit => {
                                    tracing::info!(block = ctx.block_number,
                                                   txs = executed_txs.len(),
                                                   "tx limit reached → sealing");
                                        break
                                    },
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                let is_l1 = tx.tx_type() == ZkTxType::L1;
                                if is_l1 {
                                    return Err(anyhow!("invalid l1 tx: {e:?}"));
                                } else {
                                    match command.invalid_tx_policy {
                                        InvalidTxPolicy::RejectAndContinue => {
                                            let rejection_method = rejection_method(e.clone());

                                            // mark the tx as invalid regardless of the `rejection_method`.
                                            command.tx_source.as_mut().mark_last_tx_as_invalid();
                                            // add tx to `purged_txs` only if we are purging it.
                                            match rejection_method {
                                                TxRejectionMethod::Purge => {
                                                    purged_txs.push((*tx.hash(), e.clone()));
                                                    tracing::warn!(block = ctx.block_number, ?e, "invalid tx → purged");
                                                }
                                                TxRejectionMethod::Skip => {
                                                    tracing::warn!(block = ctx.block_number, ?e, "invalid tx → skipped");
                                                },
                                                TxRejectionMethod::SealBlock => {
                                                    tracing::info!(block = ctx.block_number, ?e, "sealing block by criterion");
                                                    break;
                                                }
                                            }
                                        }
                                        InvalidTxPolicy::Abort => {
                                            return Err(anyhow!("invalid l2 tx: {e:?}"));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    /* ----- tx cannot be included -------------------------- */
                    Some(tx) => {
                        if matches!(command.seal_policy, SealPolicy::UntilExhausted) {
                            anyhow::bail!("tx {} cannot be included in block {}: cumulative gas {} exceeds block gas limit {}",
                                tx.hash(),
                                ctx.block_number,
                                cumulative_gas_used + tx.inner.gas_limit(),
                                ctx.gas_limit
                            );
                        }

                        tracing::info!(block = ctx.block_number, "sealing block as next tx cannot be included");
                        break;
                    }
                    /* ----- tx stream exhausted  --------------------------- */
                    None => {
                        if executed_txs.is_empty() && matches!(command.seal_policy, SealPolicy::UntilExhausted)
                        {
                            // Replay path requires at least one tx.

                            // todo: maybe put this check to `ReplayRecord` instead - and just assert here? Or not even assert.
                            return Err(anyhow!(
                                "empty replay for block {}",
                                ctx.block_number
                            ));
                        }

                        tracing::info!(
                            block = ctx.block_number,
                            txs = executed_txs.len(),
                            "stream exhausted → sealing"
                        );
                        break;
                    }
                }
            }
        }
    }
    let seal_latency_observer = EXECUTION_METRICS.block_execution_stages[&"seal"].start();

    /* ---------- seal & return ------------------------------------- */
    let output = runner
        .seal_block()
        .await
        .map_err(|e| anyhow!("VM seal failed: {e:?}"))?;
    EXECUTION_METRICS
        .storage_writes_per_block
        .observe(output.storage_writes.len() as u64);
    seal_latency_observer.observe();

    let block_hash_output = hash_block_output(&output);

    // Check if the block output matches the expected hash.
    if let Some(expected_hash) = command.expected_block_output_hash {
        anyhow::ensure!(
            expected_hash == block_hash_output,
            "Block #{} output hash mismatch: expected {expected_hash}, got {block_hash_output}",
            ctx.block_number,
        );
    }

    Ok((
        output,
        ReplayRecord::new(
            ctx,
            command.starting_l1_priority_id,
            executed_txs,
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
    // block is out of some resource, so it should be sealed
    SealBlock,
}

fn rejection_method(error: InvalidTransaction) -> TxRejectionMethod {
    match error {
        InvalidTransaction::InvalidEncoding
        | InvalidTransaction::InvalidStructure
        | InvalidTransaction::PriorityFeeGreaterThanMaxFee
        | InvalidTransaction::BaseFeeGreaterThanMaxFee
        | InvalidTransaction::CallerGasLimitMoreThanBlock
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
        | InvalidTransaction::UpgradeTxFailed => TxRejectionMethod::Purge,

        InvalidTransaction::GasPriceLessThanBasefee
        | InvalidTransaction::LackOfFundForMaxFee { .. }
        | InvalidTransaction::NonceTooHigh { .. } => TxRejectionMethod::Skip,

        InvalidTransaction::BlockGasLimitReached
        | InvalidTransaction::BlockNativeLimitReached
        | InvalidTransaction::BlockPubdataLimitReached
        | InvalidTransaction::BlockL2ToL1LogsLimitReached => TxRejectionMethod::SealBlock,
    }
}
