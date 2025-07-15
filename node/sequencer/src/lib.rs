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
mod metrics;
pub mod model;
pub mod prover_api;
pub mod repositories;
pub mod reth_state;
pub mod tree_manager;

use crate::api::run_jsonrpsee_server;
use crate::batcher::Batcher;
use crate::block_replay_storage::{BlockReplayColumnFamily, BlockReplayStorage};
use crate::config::{BatcherConfig, MempoolConfig, ProverApiConfig, RpcConfig, SequencerConfig};
use crate::finality::FinalityTracker;
use crate::metrics::GENERAL_METRICS;
use crate::model::BatchJob;
use crate::prover_api::proof_storage::{ProofColumnFamily, ProofStorage};
use crate::prover_api::prover_job_manager::ProverJobManager;
use crate::prover_api::prover_server;
use crate::repositories::RepositoryManager;
use crate::reth_state::ZkClient;
use crate::tree_manager::TreeManager;
use crate::{
    block_context_provider::BlockContextProvider,
    execution::{
        block_executor::execute_block, block_transactions_provider::BlockTransactionsProvider,
    },
    model::{BlockCommand, ReplayRecord},
};
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use futures::stream::{BoxStream, StreamExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;
use tokio::time::Instant;
use zk_os_forward_system::run::{BatchOutput as BlockOutput, BatchOutput};
use zksync_os_l1_sender::config::L1SenderConfig;
use zksync_os_l1_sender::{L1Sender, L1SenderHandle};
use zksync_os_l1_watcher::{L1Watcher, L1WatcherConfig};
use zksync_os_merkle_tree::MerkleTree;
use zksync_os_state::{StateConfig, StateHandle};
use zksync_os_types::forced_deposit_transaction;
use zksync_storage::RocksDB;
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

const BLOCK_REPLAY_WAL_DB_NAME: &str = "block_replay_wal";
const TREE_DB_NAME: &str = "tree";
const PROOF_STORAGE_DB_NAME: &str = "proofs";

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
            "▶ Added to state in {:?}. Reporting back to block_transaction_provider (mempools)...",
            stage_started_at.elapsed()
        );
        stage_started_at = Instant::now();

        // TODO: would updating mempool in parallel with state make sense?
        block_transaction_provider.on_canonical_state_change(&block_output, &replay_record);

        tracing::info!(
            block_number = bn,
            "▶ Reported to block_transaction_provider in {:?}. Adding to repos...",
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

        GENERAL_METRICS.block_number[&"execute"].set(bn);
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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    stop_receiver: watch::Receiver<bool>,
    rpc_config: RpcConfig,
    mempool_config: MempoolConfig,
    sequencer_config: SequencerConfig,
    l1_sender_config: L1SenderConfig,
    l1_watcher_config: L1WatcherConfig,
    batcher_config: BatcherConfig,
    prover_api_config: ProverApiConfig,
) {
    let block_replay_storage_rocks_db = RocksDB::<BlockReplayColumnFamily>::new(
        &sequencer_config
            .rocks_db_path
            .join(BLOCK_REPLAY_WAL_DB_NAME),
    )
    .expect("Failed to open BlockReplayWAL")
    .with_sync_writes();

    let block_replay_storage = BlockReplayStorage::new(block_replay_storage_rocks_db);

    let state_handle = StateHandle::new(StateConfig {
        // when running batcher, we need to start from zero due to in-memory tree
        erase_storage_on_start: batcher_config.component_enabled,
        blocks_to_retain_in_memory: sequencer_config.blocks_to_retain_in_memory,
        rocks_db_path: sequencer_config.rocks_db_path.clone(),
    });

    let repositories = RepositoryManager::new(sequencer_config.blocks_to_retain_in_memory);

    let proof_storage_db = RocksDB::<ProofColumnFamily>::new(
        &sequencer_config.rocks_db_path.join(PROOF_STORAGE_DB_NAME),
    )
    .expect("Failed to open ProofStorageDB");

    let proof_storage = ProofStorage::new(proof_storage_db);
    tracing::info!(
        "Proof storage initialized with {} proofs already present",
        proof_storage.get_blocks_with_proof().len()
    );

    // =========== load last persisted block numbers.  ===========
    let (storage_map_block, preimages_block) = state_handle.latest_block_numbers();
    let wal_block = block_replay_storage.latest_block();

    // todo: will be used once repositories have persistence
    // let repository_blocks = ...

    // it's enough to check a weaker condition (`>= min`) - but currently neither state components can be ahead of WAL
    assert!(
        wal_block.unwrap_or(0) >= storage_map_block,
        "State DB block number ({storage_map_block}) is greater than WAL block ({wal_block:?}). Preimages block: ({preimages_block})"
    );

    // ======= Initialize async channels  ===========

    // todo: this is received by batcher and then fanned out to workers. Could just use broadcast instead
    let (blocks_for_batcher_sender, blocks_for_batcher_receiver) =
        tokio::sync::mpsc::channel::<(BatchOutput, ReplayRecord)>(100);

    let (batch_sender, mut batch_receiver) = tokio::sync::mpsc::channel::<BatchJob>(100);

    let (tree_sender, tree_receiver) = tokio::sync::mpsc::channel::<BatchOutput>(100);

    let (tree_ready_block_sender, tree_ready_block_receiver) = watch::channel(0u64);

    // ========== Initialize tree manager ===========

    let tree_wrapper = TreeManager::tree_wrapper(Path::new(
        &sequencer_config.rocks_db_path.join(TREE_DB_NAME),
    ));
    let tree_manager =
        TreeManager::new(tree_wrapper.clone(), tree_receiver, tree_ready_block_sender);

    let tree_last_processed_block = tree_manager
        .last_processed_block()
        .expect("cannot read tree last processed block after initialization");

    let first_block_to_execute = if batcher_config.component_enabled {
        1
    } else {
        [
            storage_map_block,
            preimages_block,
            tree_last_processed_block,
        ]
        .iter()
        .min()
        .unwrap()
            + 1
    };

    // ========== Initialize block finality trackers ===========

    // note: unfinished feature, not really used yet
    let finality_tracker = FinalityTracker::new(wal_block.unwrap_or(0));

    tracing::info!(
        storage_map_block = storage_map_block,
        preimages_block = preimages_block,
        wal_block = wal_block,
        canonized_block = finality_tracker.get_canonized_block(),
        tree_last_processed_block = tree_last_processed_block,
        "▶ Storage read. Node starting with block {}",
        first_block_to_execute
    );

    let (l1_mempool, l2_mempool) = zksync_os_mempool::in_memory(
        ZkClient::new(repositories.clone()),
        forced_deposit_transaction(),
        mempool_config.max_tx_input_bytes,
    );

    // ========== Initialize TransactionStreamProvider ===========

    let next_l1_priority_id = block_replay_storage
        .get_replay_record(first_block_to_execute)
        .map_or(0, |block| block.starting_l1_priority_id);
    let tx_stream_provider =
        BlockTransactionsProvider::new(next_l1_priority_id, l1_mempool.clone(), l2_mempool.clone());

    // ========== Initialize L1 watcher (fallible) ===========

    let l1_watcher = L1Watcher::new(l1_watcher_config, l1_mempool).await;
    let l1_watcher_task: BoxFuture<anyhow::Result<()>> = match l1_watcher {
        Ok(l1_watcher) => Box::pin(l1_watcher.run()),
        Err(err) => {
            tracing::error!(?err, "failed to start L1 watcher; proceeding without it");
            let mut stop_receiver = stop_receiver.clone();
            Box::pin(async move {
                // Defer until we receive stop signal, i.e. a task that does nothing
                stop_receiver
                    .changed()
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
            })
        }
    };

    // ========== Initialize L1 sender (fallible) ===========

    let l1_sender = L1Sender::new(l1_sender_config).await;
    let (l1_sender_task, l1_sender_handle, _last_committed_batch_number): (
        BoxFuture<anyhow::Result<()>>,
        Option<L1SenderHandle>,
        u64,
    ) = match l1_sender {
        Ok((l1_sender, l1_handle, last_committed_batch_number)) => (
            Box::pin(l1_sender.run()),
            Some(l1_handle),
            last_committed_batch_number,
        ),
        Err(err) => {
            // todo: this is inconsistent with the behaviour when prover api / batcher are disabled:
            //  * here, we return `l1_sender_handle` as `None` and upstream components don't send to it
            //  * in prover api and batcher, we still have the channel defined and
            //    we run a dummy consumer instead of the actual component
            //  **Choose one approach to optional components and stick to it**
            tracing::error!(?err, "failed to start L1 sender; proceeding without it");
            let mut stop_receiver = stop_receiver.clone();
            (
                Box::pin(async move {
                    // Defer until we receive stop signal, i.e. a task that does nothing
                    stop_receiver
                        .changed()
                        .await
                        .map_err(|e| anyhow::anyhow!(e))
                }),
                None,
                0,
            )
        }
    };

    // ========== Initialize batcher (aka prover_input_generator) (if configured) ===========
    let batcher_task: BoxFuture<anyhow::Result<()>> = if batcher_config.component_enabled {
        // TODO: Start from `last_committed_batch_number`
        let batcher = Batcher::new(
            blocks_for_batcher_receiver,
            batch_sender,
            l1_sender_handle,
            tree_ready_block_receiver,
            state_handle.clone(),
            MerkleTree::new(tree_wrapper.clone()).expect("cannot init MerkleTreeReader"),
            batcher_config.logging_enabled,
            batcher_config.maximum_in_flight_blocks,
        );
        Box::pin(batcher.run_loop())
    } else {
        tracing::info!(
            "Batcher disabled via configuration; draining channel to avoid backpressure"
        );
        let mut receiver = blocks_for_batcher_receiver;
        Box::pin(async move {
            while receiver.recv().await.is_some() {
                // Drop messages silently to prevent backpressure
            }
            Ok(())
        })
    };

    // ======= Initialize Prover Api Server (if configured) ========

    let (prover_job_manager_task, prover_server_task): (
        BoxFuture<anyhow::Result<()>>,
        BoxFuture<anyhow::Result<()>>,
    ) = if prover_api_config.component_enabled {
        let prover_job_manager = Arc::new(ProverJobManager::new(
            proof_storage,
            prover_api_config.job_timeout,
            prover_api_config.max_unproved_blocks,
        ));
        (
            Box::pin(prover_server::run(
                prover_job_manager.clone(),
                prover_api_config.address,
            )),
            Box::pin(prover_job_manager.listen_for_batch_jobs(batch_receiver)),
        )
    } else {
        tracing::info!("Prover API via configuration; draining channel to avoid backpressure");
        let mut stop_receiver = stop_receiver.clone();
        (
            Box::pin(async move {
                while batch_receiver.recv().await.is_some() { // Drop messages silently to prevent backpressure
                }
                Ok(())
            }),
            Box::pin(async move {
                // Defer until we receive stop signal, i.e. a task that does nothing
                stop_receiver
                    .changed()
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
            }),
        )
    };
    // ======= Run tasks ===========

    tokio::select! {
        // ── Sequencer task ───────────────────────────────────────────────
        res = run_sequencer_actor(
            first_block_to_execute,
            blocks_for_batcher_sender,
            tree_sender,
            tx_stream_provider,
            state_handle.clone(),
            block_replay_storage.clone(),
            repositories.clone(),
            finality_tracker.clone(),
            sequencer_config
        ) => {
            match res {
                Ok(_)  => tracing::warn!("Sequencer server unexpectedly exited"),
                Err(e) => tracing::error!("Sequencer server failed: {e:#}"),
            }
        }


        // todo: only start after the sequencer caught up?
        // ── JSON-RPC task ────────────────────────────────────────────────
        res = run_jsonrpsee_server(
            rpc_config,
            repositories.clone(),
            finality_tracker.clone(),
            state_handle.clone(),
            l2_mempool,
            block_replay_storage.clone()
        ) => {
            match res {
                Ok(_)  => tracing::warn!("JSON-RPC server unexpectedly exited"),
                Err(e) => tracing::error!("JSON-RPC server failed: {e:#}"),
            }
        }

        // ── TREE task ────────────────────────────────────────────────
        res = tree_manager.run_loop() => {
            match res {
                Ok(_)  => tracing::warn!("TREE server unexpectedly exited"),
                Err(e) => tracing::error!("TREE server failed: {e:#}"),
            }
        }

        // ── BATCHER task (may be disabled) ──────────────────────────────────
        res = batcher_task => {
            match res {
                Ok(_)  => tracing::warn!("Batcher task exited"),
                Err(e) => tracing::error!("Batcher task failed: {e:#}"),
            }
        }

        // ── Prover Server tasks (todo: should be conditioned) ──────────────────────────────────
        _res = prover_job_manager_task => {
            tracing::warn!("Prover job manager task exited")
        }

        res = prover_server_task => {
            match res {
                Ok(_)  => tracing::warn!("prover_server_job task exited"),
                Err(e) => tracing::error!("prover_server_job task failed: {e:#}"),
            }

        }

        // ── L1 Watcher task ────────────────────────────────────────────────
        res = l1_watcher_task => {
            match res {
                Ok(_)  => tracing::warn!("L1 watcher unexpectedly exited"),
                Err(e) => tracing::error!("L1 watcher failed: {e:#}"),
            }
        }

        // ── L1 sender task ────────────────────────────────────────────────
        res = l1_sender_task => {
            match res {
                Ok(_)  => tracing::warn!("L1 sender unexpectedly exited"),
                Err(e) => tracing::error!("L1 sender failed: {e:#}"),
            }
        }

        _ = state_handle.collect_state_metrics(Duration::from_secs(2)) => {
            tracing::warn!("collect_state_metrics unexpectedly exited")
        }
        _ = state_handle.compact_periodically(Duration::from_millis(100)) => {
            tracing::warn!("compact_periodically unexpectedly exited")
        }
    }
}
