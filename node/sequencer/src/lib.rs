#![feature(allocator_api)]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

pub mod api;
pub mod batcher;
pub mod block_context_provider;
pub mod block_replay_storage;
pub mod config;
pub mod execution;
pub mod finality;
pub mod model;
pub mod prover_api;
pub mod repositories;
pub mod reth_state;
pub mod tree_manager;

use crate::block_replay_storage::BlockReplayStorage;
use crate::config::SequencerConfig;
use crate::finality::FinalityTracker;
use crate::repositories::RepositoryManager;
use crate::{
    block_context_provider::BlockContextProvider,
    execution::{
        block_executor::execute_block, block_transactions_provider::BlockTransactionsProvider,
        metrics::EXECUTION_METRICS,
    },
    model::{BlockCommand, ReplayRecord},
};
use anyhow::{Context, Result};
use futures::stream::{BoxStream, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::time::Instant;
use zk_os_forward_system::run::BatchOutput as BlockOutput;
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

    batcher_sink: Sender<(BlockOutput, ReplayRecord)>,
    tree_sink: Sender<BlockOutput>,

    mut block_transaction_provider: BlockTransactionsProvider,
    state: StateHandle,
    wal: BlockReplayStorage,
    // todo: do it outside - as one of the sinks (only needed in API)
    repositories: RepositoryManager,
    finality_tracker: FinalityTracker,
    sequencer_config: SequencerConfig,
) -> Result<()> {
    tracing::info!(block_to_start, "starting sequencer");

    let mut stream = command_source(
        &wal,
        block_to_start,
        sequencer_config.block_time,
        sequencer_config.max_transactions_in_block,
    );

    while let Some(cmd) = stream.next().await {
        // todo: also report full latency between command invocations
        let bn = cmd.block_number();

        tracing::info!(
            block = bn,
            cmd = cmd.to_string(),
            "▶ starting command. Turning into PreparedCommand.."
        );
        let mut stage_started_at = Instant::now();

        // note: block_transaction_provider has internal mutable state: `last_processed_l1_command`
        let prepared_cmd = block_transaction_provider.process_command(cmd)?;

        tracing::info!(
            block_number = bn,
            starting_l1_priority_id = prepared_cmd.starting_l1_priority_id,
            "▶ Prepared command in {:?}. Executing..",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        let (block_output, replay_record): (BlockOutput, ReplayRecord) =
            execute_block(prepared_cmd, state.clone())
                .await
                .context("execute_block")?;

        tracing::info!(
            block_number = bn,
            l1_transactions = replay_record.l1_transactions.len(),
            l2_transactions = replay_record.l2_transactions.len(),
            preimages = block_output.published_preimages.len(),
            storage_writes = block_output.storage_writes.len(),
            "▶ Executed in {:?}. Adding to state...",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        state.add_block_result(
            bn,
            block_output.storage_writes.clone(),
            block_output
                .published_preimages
                .iter()
                .map(|(k, v, _)| (*k, v)),
        )?;

        tracing::info!(
            block_number = bn,
            "▶ Added to state in {:?}. Adding to repos...",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        // TODO: update mempool in parallel with state
        block_transaction_provider.on_canonical_state_change(&block_output, &replay_record);

        tracing::info!(
            block_number = bn,
            "▶ Added to repos in {:?}. Adding to wal...",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        // todo:  this is only used by api - can and should be done async, as one of the sink subscribers
        //  but need to be careful with `block_number=latest` then -
        //  we need to make sure that the block is added to the repos before we return its number as `latest`
        //
        repositories.add_block_output_to_repos(
            bn,
            block_output.clone(),
            replay_record.l1_transactions.clone(),
            replay_record.l2_transactions.clone(),
        );

        tracing::info!(
            block_number = bn,
            "▶ Added to repos in {:?}. Adding to wal...",
            stage_started_at.elapsed()
        );

        stage_started_at = Instant::now();

        wal.append_replay(replay_record.clone());
        finality_tracker.advance_canonized(bn);

        tracing::info!(
            block_number = bn,
            "▶ Added to wal and canonized in {:?}. Sending to sinks...",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        batcher_sink
            .send((block_output.clone(), replay_record))
            .await?;
        tree_sink.send(block_output).await?;

        tracing::info!(
            block_number = bn,
            "✔ sent to sinks in {:?}",
            stage_started_at.elapsed()
        );

        EXECUTION_METRICS.sealed_block[&"execute"].set(bn);
    }
    Ok::<(), anyhow::Error>(())
}

/// In decentralized case, consensus will provide a stream of
/// interleaved Replay and Produce commands instead.
/// Currently it's a stream of Replays followed by Produces
fn command_source(
    block_replay_wal: &BlockReplayStorage,
    block_to_start: u64,
    block_time: Duration,
    block_size_limit: usize,
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
                    .get_produce_command(block_number, block_time, block_size_limit)
                    .await,
                block_number + 1,
            ))
        })
        .boxed();
    let stream = replay_wal_stream.chain(produce_stream);
    stream.boxed()
}
