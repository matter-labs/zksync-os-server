#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

pub mod api;
pub mod batcher;
pub mod block_context_provider;
pub mod block_replay_storage;
pub mod commitment;
pub mod config;
mod conversions;
pub mod execution;
pub mod finality;
pub mod model;
pub mod repositories;
pub mod tree_manager;

use crate::block_replay_storage::BlockReplayStorage;
use crate::config::SequencerConfig;
use crate::finality::FinalityTracker;
use crate::repositories::RepositoryManager;
use crate::{
    block_context_provider::BlockContextProvider,
    execution::{block_executor::execute_block, metrics::EXECUTION_METRICS},
    model::{BlockCommand, ReplayRecord},
};
use anyhow::{Context, Result};
use futures::stream::{BoxStream, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;
use zk_os_forward_system::run::BatchOutput;
use zksync_os_mempool::DynPool;
use zksync_os_state::StateHandle;
// Terms:
// * BlockReplayData     - minimal info to (re)apply the block.
//
// * `Canonize` block operation   - after block is processed in the VM, we want to expose it in API asap.
//                         But we also want to make it durable - that is, it will not change after node crashes or restarts.
//
//                         Canonization is the process of making a block durable.
//                         For the centralized sequencer we persist a WAL of `BlockReplayData`s
//                         For leader rotation we only consider block Canonized when it's accepted by the network (we have quorum).

// Minimal sequencer state is the following:
// * State (storage logs + factory deps - see `state` crate)
// * WAL of BlockReplayData (only in centralized case)
//
// Note that for API additional data may be reqiured (block/tx receipts)

const CHAIN_ID: u64 = 270;

// todo: clean up list of fields passed; split into two function (canonization and sequencing)

#[allow(clippy::too_many_arguments)]
pub async fn run_sequencer_actor(
    block_to_start: u64,

    batcher_sink: Sender<(BatchOutput, ReplayRecord)>,
    tree_sink: Sender<BatchOutput>,

    mempool: DynPool,
    state: StateHandle,
    wal: BlockReplayStorage,
    repositories: RepositoryManager,
    finality_tracker: FinalityTracker,
    sequencer_config: SequencerConfig,
) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(BatchOutput, ReplayRecord)>(2);

    /* ------------------------------------------------------------------ */
    /*  Stage 1: execute VM and send results                              */
    /* ------------------------------------------------------------------ */
    let exec_loop = {
        let wal = wal.clone();
        let mempool = mempool.clone();
        let state = state.clone();

        async move {
            let mut stream = command_source(&wal, block_to_start, sequencer_config.block_time);

            while let Some(cmd) = stream.next().await {
                // todo: also report full latency between command invocations
                let bn = cmd.block_number();

                let mut stage_started_at = Instant::now();
                tracing::info!(block = bn, cmd = cmd.to_string(), "▶ starting command");
                let (batch_out, replay) = execute_block(
                    cmd,
                    Box::into_pin(mempool.clone()),
                    state.clone(),
                    sequencer_config.max_transactions_in_block,
                )
                .await
                .context("execute_block")?;
                tracing::info!(
                    block_number = bn,
                    transactions = replay.transactions.len(),
                    preimages = batch_out.published_preimages.len(),
                    storage_writes = batch_out.storage_writes.len(),
                    "▶ Executed in {:?}. Adding to state...",
                    stage_started_at.elapsed()
                );

                stage_started_at = Instant::now();
                // important: this cannot be done async.
                // to start next block, we need to have the StorageView as of the end of previous block.
                state.add_block_result(
                    bn,
                    batch_out.storage_writes.clone(),
                    batch_out
                        .published_preimages
                        .iter()
                        .map(|(k, v, _)| (*k, v)),
                )?;
                tracing::info!(
                    block_number = bn,
                    "▶ Added to state in {:?}. Sending to canonise queue...",
                    stage_started_at.elapsed()
                );
                tx.send((batch_out, replay))
                    .await
                    .map_err(|_| anyhow::anyhow!("canonise loop stopped"))?;

                EXECUTION_METRICS.sealed_block[&"execute"].set(bn);
            }
            Ok::<(), anyhow::Error>(())
        }
    };

    /* ------------------------------------------------------------------ */
    /*  Stage 2: canonise                                                 */
    /* ------------------------------------------------------------------ */
    let canonise_loop = async {
        while let Some((batch_out, replay)) = rx.recv().await {
            let bn = batch_out.header.number;

            let mut stage_started_at = Instant::now();
            tracing::info!(block = bn, "▶ Starting canonization - adding to repos...");

            // todo:  this is only used by api - can and should be done async,
            //  but need to be careful with `block_number=latest` then -
            //  we need to make sure that the block is added to the repos before we return its number as `latest`

            repositories.add_block_output_to_repos(
                bn,
                batch_out.clone(),
                replay.transactions.clone(),
            );

            tracing::info!(
                block_number = bn,
                "▶ Added to repos in {:?}. Adding to wal...",
                stage_started_at.elapsed()
            );

            stage_started_at = Instant::now();
            wal.append_replay(replay.clone());
            finality_tracker.advance_canonized(bn);

            tracing::info!(
                block_number = bn,
                "▶ Added to wal and canonized in {:?}. Sending to state diffs sink...",
                stage_started_at.elapsed()
            );

            stage_started_at = Instant::now();

            batcher_sink.send((batch_out.clone(), replay)).await?;
            tree_sink.send(batch_out).await?;

            tracing::info!(
                block_number = bn,
                "✔ sent to sinks in {:?}",
                stage_started_at.elapsed()
            );

            EXECUTION_METRICS.sealed_block[&"canonize"].set(bn);
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        res = exec_loop      => res?,
        res = canonise_loop  => res?,
    }
    Ok(())
}

fn command_source(
    block_replay_wal: &BlockReplayStorage,
    block_to_start: u64,
    block_time: Duration,
) -> BoxStream<BlockCommand> {
    let last_block_in_wal = block_replay_wal.latest_block().unwrap_or(0);
    tracing::info!(last_block_in_wal, "Last block in WAL: {last_block_in_wal}");
    tracing::info!(block_to_start, "block_to_start: {block_to_start}");

    // Stream of replay commands from WAL
    let replay_wal_stream: BoxStream<BlockCommand> =
        Box::pin(block_replay_wal.replay_commands_from(block_to_start));

    // Combined source: run WAL replay first, then produce blocks from mempool
    let produce_stream: BoxStream<BlockCommand> =
        futures::stream::unfold(last_block_in_wal + 1, move |block_number| async move {
            Some((
                BlockContextProvider
                    .get_produce_command(block_number, block_time)
                    .await,
                block_number + 1,
            ))
        })
        .boxed();
    let stream = replay_wal_stream.chain(produce_stream);
    stream.boxed()
}
