use crate::config::SequencerConfig;
use crate::execution::block_context_provider::BlockContextProvider;
use crate::execution::execute_block_in_vm::execute_block_in_vm;
use crate::execution::metrics::{EXECUTION_METRICS, SequencerState};
use crate::execution::utils::save_dump;
use crate::model::blocks::{BlockCommand, BlockCommandType, BlockPayload};
use anyhow::Context;
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use zksync_os_mempool::subpools::l2::L2Subpool;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_storage_api::{OverlayBuffer, ReadStateHistory, WriteState};
use zksync_os_tx_validators::deployment_filter;
use zksync_os_tx_validators::policy_client::AccessType;
use zksync_os_types::{NotAcceptingReason, TransactionAcceptanceState};

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
    pub applied_block_number_receiver: watch::Receiver<u64>,
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
            if let Some(last_block_number) = self.block_context_provider.last_block_number() {
                wait_for_block_applier(&mut self.applied_block_number_receiver, last_block_number)
                    .await?;
            }

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

            let Some(prepared_command) = self.block_context_provider.prepare_command(cmd).await?
            else {
                continue;
            };

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

            let exec_view = state_overlay_buffer
                .sync_with_base_and_build_view_for_block(&self.state, block_number)?;

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

            self.block_context_provider
                .on_canonical_state_change(
                    block_output.as_ref(),
                    &replay_record,
                    strict_subpool_cleanup,
                )
                .await;
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

            output.send_and_record(
                BlockPayload {
                    output: block_output,
                    record: replay_record,
                    command_type: cmd_type,
                    failed_transactions: purged_txs,
                },
                &state_reporter,
            )?;
        }
    }
}

async fn wait_for_block_applier(
    applied_block_number_receiver: &mut watch::Receiver<u64>,
    required_block_number: u64,
) -> anyhow::Result<()> {
    let applied_block_number = *applied_block_number_receiver.borrow_and_update();
    if applied_block_number >= required_block_number {
        tracing::debug!(
            applied_block_number,
            required_block_number,
            "BlockExecutor does not need to wait for BlockApplier"
        );
        return Ok(());
    }

    tracing::debug!(
        applied_block_number,
        required_block_number,
        "BlockExecutor waiting for BlockApplier to catch up"
    );

    let reached_block_number = applied_block_number_receiver
        .wait_for(|block_number| *block_number >= required_block_number)
        .await
        .context("block applier progress watch closed while executor was waiting")?
        .to_owned();

    tracing::debug!(
        reached_block_number,
        required_block_number,
        "BlockExecutor resumed after BlockApplier caught up"
    );
    Ok(())
}

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
