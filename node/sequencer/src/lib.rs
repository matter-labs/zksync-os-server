pub mod api;
pub mod block_context_provider;
pub mod block_replay_storage;
pub mod config;
mod conversions;
pub mod execution;
pub mod finality;
pub mod mempool;
pub mod model;
pub mod repositories;
pub mod tree_manager;

use crate::block_replay_storage::BlockReplayStorage;
use crate::config::SequencerConfig;
use crate::repositories::RepositoryManager;
use crate::{
    block_context_provider::BlockContextProvider,
    execution::{block_executor::execute_block, metrics::EXECUTION_METRICS},
    mempool::Mempool,
    model::{BlockCommand, ReplayRecord},
};
use anyhow::{Context, Result};
use futures::stream::{BoxStream, StreamExt};
use std::time::Duration;
use tokio::time::Instant;
use zk_os_forward_system::run::BatchOutput;
use zksync_os_state::StateHandle;
use crate::finality::FinalityTracker;
// Terms:
// * BlockReplayData     - minimal info to (re)apply the block.
//
// * `Canonize` block operation   - after block is processed in the VM, we want to expose it in API asap.
//                         But we can only do that once it's durable - that is, it will not change after node crash/restart.
//                         Canonization is the process of making a block durable.
//                         For the centralized sequencer we persist a WAL of `BlockReplayData`s
//                         For leader rotation we only consider block Canonized when it's accepted by the network (we have quorum)

// Node state consists of three things:

// * State (storage logs + factory deps)
// * BlockReceipts (events + pubdata + other heavy info)
// * TxReceipts (events + pubdata + other heavy info)
// * WAL of BlockReplayData (only in centralized case)
//
// Only canonized blocks are ever persisted and/or exposed to API
const CHAIN_ID: u64 = 270;

// todo: consider splitting block production from canonization to own methods
pub async fn run_sequencer_actor(
    block_to_start: u64,
    mempool: Mempool,
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
                tracing::info!(block = bn, cmd = cmd.to_string() , "▶ starting - executing...");
                let (batch_out, replay) =
                    execute_block(cmd, Box::pin(mempool.clone()), state.clone())
                        .await
                        .context("execute_block")?;
                tracing::info!(
                    block_number = bn,
                    transactions = replay.transactions.len(),
                    preimages = batch_out.published_preimages.len(),
                    storage_writes = batch_out.storage_writes.len(),
                    took =? stage_started_at.elapsed(),
                    "▶ Executed! next: adding to state...",
                );

                stage_started_at = Instant::now();
                state.add_block_result(
                    bn,
                    batch_out.storage_writes.clone(),
                    batch_out.published_preimages.iter().map(|(k, v, _)| (*k, v)),
                )?;
                tracing::info!(
                    block_number = bn,
                    took =? stage_started_at.elapsed(),
                    "▶ Added to state! next: sending to canonise queue...",
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
            tracing::info!(
                block = bn,
                transactions = replay.transactions.len(),
                "▶ Starting canonization. Next: adding to repos..."
            );
            repositories.add_block_output_to_repos(
                bn,
                batch_out.clone(),
                replay.transactions.clone(),
            );

            tracing::info!(
                block_number = bn,
                transactions = replay.transactions.len(),
                preimages = batch_out.published_preimages.len(),
                storage_writes = batch_out.storage_writes.len(),
                took =? stage_started_at.elapsed(),
                "▶ Added to repos! Next: adding to wal...",
            );

            stage_started_at = Instant::now();
            wal.append_replay(replay);

            tracing::info!(
                block_number = bn,
                took =? stage_started_at.elapsed(),
                "▶ Added to wal! Next: advancing canonized...",
            );
            finality_tracker.advance_canonized(bn);

            tracing::info!(
                block_number = bn,
                took =? stage_started_at.elapsed(),
                "✔ complete",
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
