use crate::config::SequencerConfig;
use crate::execution::block_context_provider::BlockContextProvider;
use crate::execution::execute_block_in_vm::execute_block_in_vm;
use crate::execution::metrics::{EXECUTION_METRICS, SequencerState};
use crate::execution::utils::{BlockDump, parallel_producer_profile_enabled, save_dump};
use crate::model::blocks::{
    BlockCommand, BlockCommandType, BlockOutputWithReads, BlockPayload,
};
use alloy::primitives::{BlockNumber, TxHash};
use anyhow::Context;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use zksync_os_interface::error::InvalidTransaction;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_storage_api::{OverlayBuffer, ReadStateHistory, ReplayRecord, WriteState};
use zksync_os_tx_validators::deployment_filter;
use zksync_os_tx_validators::policy_client::AccessType;
use zksync_os_types::{NotAcceptingReason, TransactionAcceptanceState};

type VmExecResult = Result<
    (
        BlockOutputWithReads,
        ReplayRecord,
        Vec<(TxHash, InvalidTransaction)>,
        bool,
    ),
    BlockDump,
>;

/// Bench-only: one spawned parallel round, bundled for `finish_parallel_round`.
struct PendingParallelRound {
    handles: Vec<tokio::task::JoinHandle<VmExecResult>>,
    cmd_type: BlockCommandType,
    spawned_at: Instant,
}

struct FinishedRoundStats {
    base_block_number: u64,
    blocks: usize,
    txs: usize,
    /// Spawn-to-join wall time. Blocks pull their txs live from the lane channels, so this
    /// includes in-block waits on the tx stream (bounded by the per-block seal deadline).
    exec_elapsed: Duration,
    downstream_elapsed: Duration,
}

/// Joins the in-flight parallel round: per block (in order) advances `last_block` and the overlay
/// buffer, purges rejected txs, and forwards the payload downstream. Must run before anything else
/// consumes `last_block` or mutates the overlay (the overlay refcount-1 invariant holds here
/// because every VM task — and thus every view clone — has completed).
async fn finish_parallel_round<Subpool: L2Subpool>(
    round: PendingParallelRound,
    block_context_provider: &mut BlockContextProvider<Subpool>,
    state_overlay_buffer: &mut OverlayBuffer,
    block_dump_path: PathBuf,
    output: &mpsc::Sender<BlockPayload>,
    state_reporter: &ComponentStateReporter,
) -> anyhow::Result<FinishedRoundStats> {
    let mut results = Vec::with_capacity(round.handles.len());
    for handle in round.handles {
        results.push(handle.await.context("execute_block_in_vm task join")?);
    }
    let exec_elapsed = round.spawned_at.elapsed();

    let t_downstream = Instant::now();
    let blocks = results.len();
    let mut txs = 0usize;
    let mut base_block_number = 0u64;
    for (index, exec_result) in results.into_iter().enumerate() {
        let (block_output, replay_record, purged_txs, _strict_subpool_cleanup) = exec_result
            .map_err(|dump| {
                let error = anyhow::anyhow!("{}", dump.error);
                if let Err(err) = save_dump(block_dump_path.clone(), dump) {
                    tracing::error!(?err, "Failed to write block dump");
                }
                error
            })
            .context("execute_block_in_vm")?;
        let block_number = replay_record.block_context.block_number;
        if index == 0 {
            base_block_number = block_number;
        }
        txs += replay_record.transactions.len();
        block_context_provider
            .on_canonical_state_change_direct(block_output.as_ref(), &replay_record);
        let purged_txs_hashes = purged_txs.iter().map(|(hash, _)| *hash).collect();
        block_context_provider.purge_transactions(purged_txs_hashes);
        state_overlay_buffer.add_block(
            block_number,
            block_output.as_ref().storage_writes.clone(),
            block_output.as_ref().published_preimages.clone(),
        )?;
        EXECUTION_METRICS.block_number.set(block_number);
        EXECUTION_METRICS
            .last_execution_version
            .set(replay_record.block_context.execution_version as u64);
        output.send_and_record(
            BlockPayload {
                output: block_output,
                record: replay_record,
                command_type: round.cmd_type,
                failed_transactions: purged_txs,
            },
            state_reporter,
        )?;
    }
    Ok(FinishedRoundStats {
        base_block_number,
        blocks,
        txs,
        exec_elapsed,
        downstream_elapsed: t_downstream.elapsed(),
    })
}

/// Executes blocks, while only updating local in-memory state (mempool, block context).
/// Does not persist anything to disk.
/// Does not track the node role - reacts on the ordered inbound commands instead (`Produce` vs `Replay`)
pub struct BlockExecutor<Subpool, State>
where
    Subpool: L2Subpool + Send + 'static,
    State: ReadStateHistory + WriteState + Clone + Send + 'static,
{
    pub block_context_provider: BlockContextProvider<Subpool>,
    pub state: State,
    pub config: SequencerConfig,
    /// Controls transaction acceptance state.
    /// When max_blocks_to_produce limit is reached, sequencer sends NotAccepting to stop RPC from accepting new txs.
    pub tx_acceptance_state_sender: watch::Sender<TransactionAcceptanceState>,
    /// TEMPORARY: `BlockExecutor` waits for `BlockApplier` to apply block `N`
    /// before starting block `N + 1`. This works around an `OverlayBuffer` bug
    /// that reproduces during rebuilds when the runtime truncates base state.
    /// Once that bug is fixed, this wait can be removed.
    pub applied_block_number_receiver: watch::Receiver<Option<BlockNumber>>,
}

#[async_trait]
impl<Subpool, State> PipelineComponent for BlockExecutor<Subpool, State>
where
    Subpool: L2Subpool + Send + 'static,
    State: ReadStateHistory + WriteState + Clone + Send + 'static,
{
    /// Input from `CommandSource`
    type Input = BlockCommand;
    /// Output to `BlockCanonizer`
    /// Outputs executed blocks. Passes along information whether it's a replayed or new block -
    ///  new blocks need to be canonized by network (enforced by `BlockCanonizer`)
    type Output = BlockPayload;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::BlockExecutor;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // Track how many Produce commands we've processed (for `sequencer_max_blocks_to_produce` config)
        let mut produced_blocks_count = 0u64;

        // Only used for metrics/logs
        let mut last_processed_block_at: Option<Instant> = None;
        // `BlockExecutor` doesn't persist/update state after block execution.
        // Instead, we keep the diff in memory - and apply it on top of the last persisted block
        let mut state_overlay_buffer = OverlayBuffer::default();
        loop {
            state_reporter.enter_state(SequencerState::WaitingForCommand);

            let Some(cmd) = input.recv().await else {
                tracing::info!("inbound channel closed");
                return Ok(());
            };
            tracing::info!("Command {cmd} received by BlockExecutor");
            let cmd_type = cmd.command_type();
            state_reporter.enter_state(SequencerState::WaitingForApplier);
            // // If we had a non-genesis block in the past, we need to wait for the block applier to
            // // catch up. Genesis is skipped because it never gets applied further down the pipeline.
            // if let Some(last_block_number) = self.block_context_provider.last_block_number()
            //     && last_block_number > 0
            // {
            //     wait_for_block_applier(&mut self.applied_block_number_receiver, last_block_number)
            //         .await?;
            // }

            // For Produce commands: check limit (will await indefinitely if limit reached) and increment counter
            if matches!(cmd, BlockCommand::Produce(_))
                && let Some(limit) = self.config.max_blocks_to_produce
            {
                check_block_production_limit(
                    limit,
                    produced_blocks_count,
                    &self.tx_acceptance_state_sender,
                    &state_reporter,
                )
                .await;
                produced_blocks_count += 1;
            }
            state_reporter.enter_state(SequencerState::WaitingForTransaction);

            // Bench-only parallel path: execute K slot-disjoint blocks concurrently against a
            // shared base state, one block per non-empty lane. Each block's tx_source is a LIVE
            // stream over its lane channel, so there is no drain step (and nothing to pipeline):
            // the channels buffer arrivals while a round executes, and the next round's blocks
            // pull them directly. Gated on direct injection, so the production (serial) path
            // stays untouched.
            if matches!(cmd, BlockCommand::Produce(_))
                && self.config.parallel_blocks > 1
                && self.block_context_provider.is_direct_active()
                && self
                    .block_context_provider
                    .has_parallel_lanes(self.config.parallel_blocks)
            {
                let k = self.config.parallel_blocks;
                // Parks internally while every lane is empty (safe: no round is in flight, so
                // receipt-waiting feeds are never blocked by the park). Empty result means every
                // lane channel is closed.
                let t_build = Instant::now();
                let commands = self
                    .block_context_provider
                    .build_parallel_lane_commands(k)
                    .await?;
                let build_elapsed = t_build.elapsed();
                if commands.is_empty() {
                    continue;
                }

                // All K disjoint blocks read the same base state at `base_block_number - 1`;
                // build one overlay-aware view and hand a clone to each block.
                let base_block_number = commands[0].block_context.block_number;
                let t_base_view = Instant::now();
                let base_view = state_overlay_buffer
                    .sync_with_base_and_build_view_for_block(&self.state, base_block_number)?;
                let base_view_elapsed = t_base_view.elapsed();
                state_reporter.enter_state(SequencerState::InitializingVm);
                let mut handles = Vec::with_capacity(commands.len());
                for command in commands {
                    let view = base_view.clone();
                    let reporter = state_reporter.clone();
                    // Direct injection only runs on the main node, so `is_produce` is
                    // always true here.
                    let (tracer, validator) =
                        make_deployment_filter(true, &self.config.tx_validator.deployment_filter);
                    handles.push(tokio::spawn(async move {
                        execute_block_in_vm(command, view, &reporter, tracer, validator).await
                    }));
                }
                // The K view clones live inside the spawned tasks; the local borrow is released
                // here so the join below can mutate the overlay (refcount back to 1).
                drop(base_view);

                let stats = finish_parallel_round(
                    PendingParallelRound {
                        handles,
                        cmd_type,
                        spawned_at: Instant::now(),
                    },
                    &mut self.block_context_provider,
                    &mut state_overlay_buffer,
                    self.config.block_dump_path.clone(),
                    &output,
                    &state_reporter,
                )
                .await?;
                last_processed_block_at = Some(Instant::now());

                if parallel_producer_profile_enabled() {
                    tracing::info!(
                        k,
                        base_block_number = stats.base_block_number,
                        blocks = stats.blocks,
                        downstream_txs = stats.txs,
                        ?build_elapsed,
                        ?base_view_elapsed,
                        exec_elapsed = ?stats.exec_elapsed,
                        downstream_elapsed = ?stats.downstream_elapsed,
                        "block_executor parallel round profile"
                    );
                } else {
                    tracing::info!(
                        k,
                        base_block_number = stats.base_block_number,
                        blocks = stats.blocks,
                        downstream_txs = stats.txs,
                        ?build_elapsed,
                        ?base_view_elapsed,
                        exec_elapsed = ?stats.exec_elapsed,
                        downstream_elapsed = ?stats.downstream_elapsed,
                        "block_executor parallel round done"
                    );
                }
                continue;
            }

            // Bench-only non-pipelined parallel fallback (shared direct channel without per-signer
            // lanes): produce + execute + flush one round at a time.
            if matches!(cmd, BlockCommand::Produce(_))
                && self.config.parallel_blocks > 1
                && self.block_context_provider.is_direct_active()
            {
                let k = self.config.parallel_blocks;
                let t_produce_parallel = Instant::now();
                let commands = self.block_context_provider.produce_parallel(k).await?;
                let produce_parallel_elapsed = t_produce_parallel.elapsed();
                if commands.is_empty() {
                    continue;
                }
                let base_block_number = commands[0].block_context.block_number;
                let t_base_view = Instant::now();
                let base_view = state_overlay_buffer
                    .sync_with_base_and_build_view_for_block(&self.state, base_block_number)?;
                let base_view_elapsed = t_base_view.elapsed();

                state_reporter.enter_state(SequencerState::InitializingVm);
                let mut handles = Vec::with_capacity(commands.len());
                for command in commands {
                    let view = base_view.clone();
                    let reporter = state_reporter.clone();
                    // Direct injection only runs on the main node, so `is_produce` is always true here.
                    let (tracer, validator) =
                        make_deployment_filter(true, &self.config.tx_validator.deployment_filter);
                    handles.push(tokio::spawn(async move {
                        execute_block_in_vm(command, view, &reporter, tracer, validator).await
                    }));
                }
                drop(base_view);
                let stats = finish_parallel_round(
                    PendingParallelRound {
                        handles,
                        cmd_type,
                        spawned_at: Instant::now(),
                    },
                    &mut self.block_context_provider,
                    &mut state_overlay_buffer,
                    self.config.block_dump_path.clone(),
                    &output,
                    &state_reporter,
                )
                .await?;
                last_processed_block_at = Some(Instant::now());
                tracing::info!(
                    k,
                    base_block_number,
                    blocks = stats.blocks,
                    downstream_txs = stats.txs,
                    ?produce_parallel_elapsed,
                    ?base_view_elapsed,
                    exec_elapsed = ?stats.exec_elapsed,
                    downstream_elapsed = ?stats.downstream_elapsed,
                    "block_executor parallel round done"
                );
                continue;
            }

            let t_prepare = Instant::now();
            let Some(prepared_command) = self.block_context_provider.prepare_command(cmd).await?
            else {
                continue;
            };
            let prepare_elapsed = t_prepare.elapsed();

            state_reporter.enter_state(SequencerState::InitializingVm);

            let block_number = prepared_command.block_context.block_number;
            state_reporter.record_picked(
                block_number,
                Some(prepared_command.block_context.timestamp),
                None,
            );
            tracing::info!(
                block_number,
                "Prepared context for block {block_number}. expected_block_output_hash: {:?}, starting_l1_priority_id: {}, timestamp: {}, execution_version: {}. Executing..",
                prepared_command.expected_block_output_hash,
                prepared_command.starting_cursors.l1_priority_id,
                prepared_command.block_context.timestamp,
                prepared_command.block_context.execution_version,
            );

            let t_sync = Instant::now();
            let exec_view = state_overlay_buffer
                .sync_with_base_and_build_view_for_block(&self.state, block_number)?;
            let sync_elapsed = t_sync.elapsed();

            let is_produce = matches!(cmd_type, BlockCommandType::Produce);
            // Policy priority: when a `PolicyClient` is configured for a
            // produce block, the policy service is the sole arbiter and the
            // deployment filter is bypassed entirely (the policy service can
            // express the same allow-list via its rules). Replay/rebuild
            // traffic was already consulted against the policy service at
            // original sequencing, so it always falls back to the deployment
            // filter (with `Unrestricted` config to avoid re-filtering).
            let policy_client = is_produce
                .then_some(self.config.tx_validator.policy_client.as_ref())
                .flatten();
            let t_exec = Instant::now();
            let exec_result = if let Some(policy_client) = policy_client {
                let policy_session = policy_client.session(AccessType::Write);
                let policy_tracer = policy_session.paired_tracer();
                execute_block_in_vm(
                    prepared_command,
                    exec_view,
                    &state_reporter,
                    policy_tracer,
                    policy_session,
                )
                .await
            } else {
                let (tracer, validator) =
                    make_deployment_filter(is_produce, &self.config.tx_validator.deployment_filter);
                execute_block_in_vm(
                    prepared_command,
                    exec_view,
                    &state_reporter,
                    tracer,
                    validator,
                )
                .await
            };
            let exec_elapsed = t_exec.elapsed();
            let (block_output, replay_record, purged_txs, strict_subpool_cleanup) = exec_result
                .map_err(|dump| {
                    let error = anyhow::anyhow!("{}", dump.error);
                    tracing::info!("Saving dump..");
                    if let Err(err) = save_dump(self.config.block_dump_path.clone(), dump) {
                        tracing::error!(?err, "Failed to write block dump");
                    }
                    error
                })
                .context("execute_block_in_vm")?;

            let time_since_last_block = last_processed_block_at
                .map(|last_processed_block_at| last_processed_block_at.elapsed());
            if let Some(time_since_last_block) = time_since_last_block {
                EXECUTION_METRICS
                    .time_since_last_block
                    .observe(time_since_last_block);
            }
            last_processed_block_at = Some(Instant::now());

            tracing::info!(block_number, "Executed. Updating mempools...");
            state_reporter.enter_state(SequencerState::UpdatingMempool);

            let t_canonical = Instant::now();
            self.block_context_provider
                .on_canonical_state_change(
                    block_output.as_ref(),
                    &replay_record,
                    strict_subpool_cleanup,
                )
                .await;
            let canonical_elapsed = t_canonical.elapsed();
            let purged_txs_hashes = purged_txs.iter().map(|(hash, _)| *hash).collect();
            self.block_context_provider
                .purge_transactions(purged_txs_hashes);

            state_overlay_buffer.add_block(
                block_number,
                block_output.as_ref().storage_writes.clone(),
                block_output.as_ref().published_preimages.clone(),
            )?;

            tracing::info!(
                block_number,
                time_since_last_block = ?time_since_last_block,
                "Block processed in `BlockExecutor`. Sending downstream..."
            );
            EXECUTION_METRICS.block_number.set(block_number);
            EXECUTION_METRICS
                .last_execution_version
                .set(replay_record.block_context.execution_version as u64);

            let t_send = Instant::now();
            output.send_and_record(
                BlockPayload {
                    output: block_output,
                    record: replay_record,
                    command_type: cmd_type,
                    failed_transactions: purged_txs,
                },
                &state_reporter,
            )?;
            let send_elapsed = t_send.elapsed();
            tracing::info!(
                block_number,
                ?prepare_elapsed,
                ?sync_elapsed,
                ?exec_elapsed,
                ?canonical_elapsed,
                ?send_elapsed,
                cycle = ?time_since_last_block,
                "block_executor loop breakdown"
            );
        }
    }
}

// async fn wait_for_block_applier(
//     applied_block_number_receiver: &mut watch::Receiver<Option<BlockNumber>>,
//     required_block_number: BlockNumber,
// ) -> anyhow::Result<()> {
//     let applied_block_number = *applied_block_number_receiver.borrow_and_update();
//     if applied_block_number >= Some(required_block_number) {
//         tracing::debug!(
//             applied_block_number,
//             required_block_number,
//             "BlockExecutor does not need to wait for BlockApplier"
//         );
//         return Ok(());
//     }
//
//     tracing::debug!(
//         applied_block_number,
//         required_block_number,
//         "BlockExecutor waiting for BlockApplier to catch up"
//     );
//
//     let reached_block_number = applied_block_number_receiver
//         .wait_for(|block_number| *block_number >= Some(required_block_number))
//         .await
//         .context("block applier progress watch closed while executor was waiting")?
//         .to_owned();
//
//     tracing::debug!(
//         reached_block_number,
//         required_block_number,
//         "BlockExecutor resumed after BlockApplier caught up"
//     );
//     Ok(())
// }

/// Checks if block production limit has been reached.
/// If limit is reached, signals to stop accepting transactions and awaits indefinitely (never returns).
/// Should only be called for Produce commands.
async fn check_block_production_limit(
    limit: u64,
    already_produced_blocks_count: u64,
    tx_acceptance_state_sender: &watch::Sender<TransactionAcceptanceState>,
    state_reporter: &ComponentStateReporter,
) {
    if already_produced_blocks_count >= limit {
        tracing::warn!(
            already_produced_blocks_count,
            limit,
            "Reached max_blocks_to_produce limit, stopping transaction acceptance"
        );

        // Signal to RPC that we're no longer accepting transactions
        let _ = tx_acceptance_state_sender.send(TransactionAcceptanceState::NotAccepting(vec![
            NotAcceptingReason::BlockProductionDisabled,
        ]));

        state_reporter.enter_state(SequencerState::ConfiguredBlockLimitReached);
        std::future::pending::<()>().await;
    }
}

fn make_deployment_filter(
    is_produce: bool,
    config: &deployment_filter::Config,
) -> (deployment_filter::Tracer, deployment_filter::Validator) {
    let filter_config = if is_produce {
        config.clone()
    } else {
        // Replay and Rebuild commands use an unrestricted config to avoid re-filtering
        // already-accepted historical blocks.
        deployment_filter::Config::Unrestricted
    };
    let unauthorized_flag = Arc::new(AtomicBool::new(false));
    let tracer = deployment_filter::Tracer::new(unauthorized_flag.clone(), filter_config);
    let validator = deployment_filter::Validator::new(unauthorized_flag);
    (tracer, validator)
}
