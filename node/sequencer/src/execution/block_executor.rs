use crate::execution::metrics::EXECUTION_METRICS;
use crate::execution::vm_wrapper::VmWrapper;
use crate::model::{BlockCommand, ReplayRecord};
use anyhow::{anyhow, Result};
use futures::{Stream, StreamExt};
use itertools::Either;
use std::{pin::Pin, time::Duration};
use tokio::time::Sleep;
use zk_os_forward_system::run::{BatchContext, BatchOutput};
use zksync_os_mempool::L2TransactionPool;
use zksync_os_mempool::{DynL1Pool, RethPool};
use zksync_os_state::StateHandle;
use zksync_os_types::{EncodableZksyncOs, L1Transaction, L2Transaction};

/// Behaviour when VM returns an InvalidTransaction error.
#[derive(Clone, Copy, Debug)]
enum InvalidTxPolicy {
    /// Invalid tx is skipped in block and discarded from mempool. Used when building a block.
    RejectAndContinue,
    /// Bubble the error up and abort the whole block. Used when replaying a block (ReplayLog / Replica / EN)
    Abort,
}

#[derive(Clone, Copy, Debug)]
enum SealPolicy {
    /// Seal non-empty blocks after deadline. Used when building a block.
    Deadline(Duration),
    /// Seal when all txs from tx source are executed. Used when replaying a block (ReplayLog / Replica / EN)
    Exhausted,
}

/// A stream of transactions that can be `await`-ed item-by-item.
type L1TxStream = Pin<Box<dyn Stream<Item = L1Transaction> + Send>>;

/// A stream of transactions that can be `await`-ed item-by-item.
type TxStream = Pin<Box<dyn Stream<Item = L2Transaction> + Send>>;

/// Build everything the VM runner needs for this command:
///   – the context (`BatchContext`)
///   – the stream (`TxStream`)
///   – the seal and invalid-tx policies
///
/// The `mempool` parameter is *used only* by `Produce`.
fn command_into_parts(
    block_command: BlockCommand,
    next_l1_priority_id: &mut u64,
    l1_mempool: &DynL1Pool,
    l2_mempool: &RethPool,
) -> (
    BatchContext,
    L1TxStream,
    TxStream,
    SealPolicy,
    InvalidTxPolicy,
) {
    match block_command {
        BlockCommand::Produce(ctx, deadline) => {
            let mut l1_transactions = Vec::new();
            while let Some(l1_tx) =
                l1_mempool.get(*next_l1_priority_id + l1_transactions.len() as u64)
            {
                l1_transactions.push(l1_tx);
            }

            (
                ctx,
                // TODO: Make stream actually look in L1 mempool during execution
                Box::pin(futures::stream::iter(l1_transactions)) as L1TxStream,
                l2_mempool.best_l2_transactions(),
                SealPolicy::Deadline(deadline),
                InvalidTxPolicy::RejectAndContinue,
            )
        }
        BlockCommand::Replay(replay) => (
            replay.context,
            Box::pin(futures::stream::iter(replay.l1_transactions)) as L1TxStream,
            Box::pin(futures::stream::iter(replay.l2_transactions)) as TxStream,
            SealPolicy::Exhausted,
            InvalidTxPolicy::Abort,
        ),
    }
}

// Note: trying to avoid introducing a `Struct` here (e.g. `struct BlockExecutor`)
// to enforce minimal state space and state transition explicitness

// side effect of this is that it's harder to pass config values (normally we'd just pass the whole config object)
// please be mindful when adding new parameters here
pub async fn execute_block(
    cmd: BlockCommand,
    next_l1_priority_id: &mut u64,
    l1_mempool: &DynL1Pool,
    l2_mempool: &RethPool,
    state: &StateHandle,
    max_transactions_in_block: usize,
) -> Result<(BatchOutput, ReplayRecord)> {
    let metrics_label = match cmd {
        BlockCommand::Produce(_, _) => "produce",
        BlockCommand::Replay(_) => "replay",
    };
    loop {
        let (ctx, l1_stream, stream, seal, invalid) =
            command_into_parts(cmd.clone(), next_l1_priority_id, l1_mempool, l2_mempool);
        let Some(res) = execute_block_inner(
            ctx,
            state,
            next_l1_priority_id,
            l1_stream,
            stream,
            seal,
            invalid,
            metrics_label,
            max_transactions_in_block,
        )
        .await?
        else {
            // TODO: Subscribe to L1/L2 mempool changes instead of polling
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        return Ok(res);
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_block_inner(
    ctx: BatchContext,
    state: &StateHandle,
    next_l1_priority_id: &mut u64,
    l1_txs: L1TxStream,
    l2_txs: TxStream,
    seal_policy: SealPolicy,
    fail_policy: InvalidTxPolicy,
    metrics_label: &'static str,
    max_transactions_in_block: usize,
) -> Result<Option<(BatchOutput, ReplayRecord)>> {
    /* ---------- VM & state ----------------------------------------- */
    let state_view = state.state_view_at_block(ctx.block_number)?;
    let mut runner = VmWrapper::new(ctx, state_view);
    let mut l1_executed = Vec::<L1Transaction>::new();
    let mut l2_executed = Vec::<L2Transaction>::new();
    // Exhaust stream of L1 txs first before taking L2 txs
    let mut txs = l1_txs.map(Either::Left).chain(l2_txs.map(Either::Right));

    /* ---------- deadline config ------------------------------------ */
    let deadline_dur = match seal_policy {
        SealPolicy::Deadline(d) => Some(d),
        SealPolicy::Exhausted => None,
    };
    let mut deadline: Option<Pin<Box<Sleep>>> = None; // will arm after 1st tx success

    /* ---------- main loop ------------------------------------------ */
    // todo: not sure tokio::select! is a good solution -
    // for example now if we deadline mid-transaction, the tx will be lost from mempool
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
                               txs = l2_executed.len(),
                               "deadline reached → sealing");
                break;                                     // leave the loop ⇒ seal
            }

            /* -------- stream branch ------------------------------- */
            maybe_tx = txs.next() => {
                match maybe_tx {
                    /* ----- got an L1 transaction ---------------------- */
                    Some(Either::Left(l1_tx)) => {
                        wait_for_tx_latency.observe();
                        let latency = EXECUTION_METRICS.block_execution_stages[&"execute"].start();
                        match runner.execute_next_tx(l1_tx.clone().encode_zksync_os()).await {
                            Ok(_res) => {
                                // tracing::info!(
                                //     block = ctx.block_number,
                                //     tx = ?tx.hash(),
                                //     res = ?res,
                                //     "L1 tx executed"
                                // );
                                latency.observe();
                                EXECUTION_METRICS.executed_transactions[&metrics_label].inc();

                                *next_l1_priority_id = l1_tx.common_data.serial_id.0 + 1;
                                l1_executed.push(l1_tx);

                                // arm the timer once, after the first successful tx
                                if deadline.is_none() {
                                    if let Some(dur) = deadline_dur {
                                        deadline = Some(Box::pin(tokio::time::sleep(dur)));
                                    }
                                }

                                // seal block if max transactions number reached
                                if l1_executed.len() + l2_executed.len() >= max_transactions_in_block {
                                    break;
                                }
                            }
                            Err(e) => {
                                // We ignore `fail_policy` here as we cannot continue after encountering
                                // an invalid L1 transaction. This essentially halts protocol.
                                anyhow::bail!("found invalid L1 tx, cannot proceed: {e:?}");
                            }
                        }
                    }
                    /* ----- got an L2 transaction ---------------------- */
                    Some(Either::Right(l2_tx)) => {
                        wait_for_tx_latency.observe();
                        let latency = EXECUTION_METRICS.block_execution_stages[&"execute"].start();
                        match runner.execute_next_tx(l2_tx.clone().encode_zksync_os()).await {
                            Ok(_res) => {
                                // tracing::info!(
                                //     block = ctx.block_number,
                                //     tx = ?tx.hash(),
                                //     res = ?res,
                                //     "L2 tx executed"
                                // );
                                latency.observe();
                                EXECUTION_METRICS.executed_transactions[&metrics_label].inc();

                                l2_executed.push(l2_tx);

                                // arm the timer once, after the first successful tx
                                if deadline.is_none() {
                                    if let Some(dur) = deadline_dur {
                                        deadline = Some(Box::pin(tokio::time::sleep(dur)));
                                    }
                                }

                                // seal block if max transactions number reached
                                if l1_executed.len() + l2_executed.len() >= max_transactions_in_block {
                                    break;
                                }
                            }
                            Err(e) => match fail_policy {
                                InvalidTxPolicy::RejectAndContinue => {
                                    tracing::warn!(block = ctx.block_number, ?e,
                                                   "invalid L2 tx → skipped");
                                }
                                InvalidTxPolicy::Abort => {
                                    return Err(anyhow!("invalid L2 tx: {e:?}"));
                                }
                            }
                        }
                    }

                    /* ----- stream ended --------------------------- */
                    None => {
                        if l1_executed.is_empty() && l2_executed.is_empty()
                        {
                            return match seal_policy {
                                SealPolicy::Deadline(_) => {
                                    Ok(None)
                                },
                                SealPolicy::Exhausted => {
                                    // Replay path requires at least one tx.
                                    Err(anyhow!(
                                        "empty replay for block {}",
                                        ctx.block_number
                                    ))
                                }
                            }
                        }

                        tracing::info!(
                            block = ctx.block_number,
                            l1_txs = l1_executed.len(),
                            l2_txs = l2_executed.len(),
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
    Ok(Some((
        output,
        ReplayRecord {
            context: ctx,
            l1_transactions: l1_executed,
            l2_transactions: l2_executed,
        },
    )))
}
