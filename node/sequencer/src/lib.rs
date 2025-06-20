pub mod api;
mod block_context_provider;
pub mod config;
mod conversions;
pub mod execution;
pub mod mempool;
pub mod model;
pub mod storage;
pub mod tree_manager;
mod tx_conversions;

use crate::config::SequencerConfig;
use crate::{
    block_context_provider::BlockContextProvider,
    execution::{block_executor::execute_block, metrics::EXECUTION_METRICS},
    mempool::Mempool,
    model::{BlockCommand, ReplayRecord},
    storage::{block_replay_storage::BlockReplayStorage, StateHandle},
};
use anyhow::{Context, Result};
use futures::stream::{BoxStream, StreamExt};
use std::time::Duration;
use zk_os_forward_system::run::BatchOutput;

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

pub async fn run_sequencer_actor(
    block_to_start: u64,
    wal: BlockReplayStorage,
    state: StateHandle,
    mempool: Mempool,
    sequencer_config: SequencerConfig,
) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(BatchOutput, ReplayRecord)>(1);

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
                let bn = cmd.block_number();
                tracing::info!(block = bn, "▶ execute");

                let (batch_out, replay) =
                    execute_block(cmd, Box::pin(mempool.clone()), state.clone())
                        .await
                        .context("execute_block")?;

                state.handle_block_output(batch_out.clone(), replay.transactions.clone());
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
            tracing::info!(block = bn, "▶ append_replay");
            wal.append_replay(replay);

            tracing::info!(block = bn, "▶ advance_canonized_block");
            state.advance_canonized_block(bn);
            EXECUTION_METRICS.sealed_block[&"canonize"].set(bn);
            tracing::info!(block = bn, "✔ done");
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
