use crate::execution::metrics::EXECUTION_METRICS;
use crate::execution::vm_wrapper::VmWrapper;
use crate::model::{
    InvalidTxPolicy, PreparedBlockCommand, ReplayRecord, SealPolicy, UnifiedTransaction,
};
use anyhow::{anyhow, Result};
use futures::StreamExt;
use itertools::{Either, Itertools};
use std::pin::Pin;
use tokio::time::Sleep;
use zk_os_forward_system::run::BatchOutput;
use zksync_os_state::StateHandle;
use zksync_os_types::{EncodableZksyncOs, L1Transaction, L2Transaction};

// Note that this is a pure function without a container struct (e.g. `struct BlockExecutor`)
// MAINTAIN this to ensure the function is completely stateless - explicit or implicit.

// a side effect of this is that it's harder to pass config values (normally we'd just pass the whole config object)
// please be mindful when adding new parameters here

pub async fn execute_block(
    mut command: PreparedBlockCommand,
    state: StateHandle,
) -> Result<(BatchOutput, ReplayRecord)> {
    let ctx = command.block_context;

    /* ---------- VM & state ----------------------------------------- */
    let state_view = state.state_view_at_block(ctx.block_number)?;
    let mut runner = VmWrapper::new(ctx, state_view);

    // todo: not sure we need them separate
    let mut executed_txs = Vec::<UnifiedTransaction>::new();

    /* ---------- deadline config ------------------------------------ */
    let deadline_dur = match command.seal_policy {
        SealPolicy::Decide(d, _) => Some(d),
        SealPolicy::UntilExhausted => None,
    };
    let mut deadline: Option<Pin<Box<Sleep>>> = None; // will arm after 1st tx success

    /* ---------- main loop ------------------------------------------ */
    loop {
        let wait_for_tx_latency = EXECUTION_METRICS.block_execution_stages[&"wait_for_tx"].start();
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
                    Some(tx) => {
                        wait_for_tx_latency.observe();
                        let latency = EXECUTION_METRICS.block_execution_stages[&"execute"].start();
                        match runner.execute_next_tx(tx.clone().encode_zksync_os()).await {
                            Ok(_res) => {
                                latency.observe();
                                EXECUTION_METRICS.executed_transactions[&command.metrics_label].inc();

                                executed_txs.push(tx);

                                // arm the timer once, after the first successful tx
                                if deadline.is_none() {
                                    if let Some(dur) = deadline_dur {
                                        deadline = Some(Box::pin(tokio::time::sleep(dur)));
                                    }
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

                                // todo: Some error types mean that the transaction is invalid but could be valid in the future (NonceTooHigh, GasPriceLessThanBasefee etc),
                                // todo: some mean it will never be valid (NonceTooLow, BaseFeeGreaterThanMaxFee etc),
                                // todo: some are in the gray zone (e.g. LackOfFundForMaxFee)
                                // todo: and some are just fatal protocol errors (e.g. UpgradeTxNotFirst, BlockGasLimitTooHigh).
                                // todo: we may apply this classification on zksync-os side or on the server side

                                // todo: we'll need to mark tx as invalid for RETH mempool - use the same hook to bail on invalid l1 transactions
                                // todo: (that is, the difference in l1/l2 logic on invalid tx will be handled in block_transactions_provider -
                                // todo: for l2 forwarded to mempool, for l1 bailed)
                                match command.invalid_tx_policy {
                                    InvalidTxPolicy::RejectAndContinue => {
                                        tracing::warn!(block = ctx.block_number, ?e,
                                                       "invalid tx → skipped");
                                    }
                                    InvalidTxPolicy::Abort => {
                                        return Err(anyhow!("invalid tx: {e:?}"));
                                    }
                                }
                            }
                        }
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
    let latency = EXECUTION_METRICS.block_execution_stages[&"seal"].start();

    /* ---------- seal & return ------------------------------------- */
    let output = runner
        .seal_batch()
        .await
        .map_err(|e| anyhow!("VM seal failed: {e:?}"))?;
    EXECUTION_METRICS
        .storage_writes_per_block
        .observe(output.storage_writes.len() as u64);
    latency.observe();

    // todo: not sure we need to track l1_ and l2_transactions separately in replay_record
    let (l1_transactions, l2_transactions): (Vec<L1Transaction>, Vec<L2Transaction>) =
        executed_txs.into_iter().partition_map(|tx| match tx {
            UnifiedTransaction::L1(tx) => Either::Left(tx),
            UnifiedTransaction::L2(tx) => Either::Right(tx),
        });

    Ok((
        output,
        ReplayRecord::new(
            ctx,
            command.starting_l1_priority_id,
            l1_transactions,
            l2_transactions,
        ),
    ))
}
