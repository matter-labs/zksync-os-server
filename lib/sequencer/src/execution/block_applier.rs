use crate::config::SequencerConfig;
use crate::execution::metrics::BlockApplierState;
use crate::model::blocks::{AppliedBlock, BlockCommandType, BlockOutputWithReads, BlockPayload};
use alloy::consensus::Sealed;
use alloy::primitives::BlockNumber;
use anyhow::Context as _;
use async_trait::async_trait;
use crate::execution::utils::parallel_producer_profile_enabled;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, PipelineComponent, SendAndRecordExt};
use zksync_os_storage_api::{
    ReplayRecord, RepositoryResult, WriteReplay, WriteRepository, WriteState,
};

/// Persists blocks in various local storages.
/// Used to be part of the Sequencer - was split into `BlockExecutor` and `BlockApplier`.
pub struct BlockApplier<State, Replay, Repo>
where
    State: WriteState + Clone + Send + 'static,
    Replay: WriteReplay + Send + 'static,
    Repo: WriteRepository + Clone + Send + 'static,
{
    pub state: State,
    pub replay: Replay,
    pub repositories: Repo,
    pub config: SequencerConfig,
    pub applied_block_number_sender: watch::Sender<Option<BlockNumber>>,
}

#[async_trait]
impl<State, Replay, Repo> PipelineComponent for BlockApplier<State, Replay, Repo>
where
    State: WriteState + Clone + Send + 'static,
    Replay: WriteReplay + Send + 'static,
    Repo: WriteRepository + Clone + Send + 'static,
{
    type Input = BlockPayload;
    type Output = AppliedBlock;

    const COMPONENT_ID: zksync_os_pipeline::ComponentId =
        zksync_os_pipeline::ComponentId::BlockApplier;

    async fn run(
        mut self,
        mut input: PeekableReceiver<Self::Input>,
        output: mpsc::Sender<Self::Output>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // Replay-WAL handling. Production (`parallel_blocks == 1`) writes inline, exactly as
        // before. Parallel mode moves the write (record clone + bincode of all txs + RocksDB
        // batch, ~2.7ms/block measured — 70% of the applier's serial budget at 190×2k-tx blocks)
        // onto a dedicated ordered writer task: the applier enqueues cheaply, and a watermark
        // gates FORWARDING downstream, so a block still never leaves the applier before its
        // replay record is durable. In-memory state/repos may transiently lead the WAL by up to
        // `parallel_blocks` blocks; nothing durable does — state compaction and repository
        // persistence trail by their retention windows.
        enum WalMode<R> {
            Inline(R),
            Pipelined {
                queue: mpsc::Sender<(BlockNumber, Sealed<Arc<ReplayRecord>>, bool)>,
                written: watch::Receiver<BlockNumber>,
            },
            Skipped,
        }
        let mut wal_mode = if self.config.parallel_blocks <= 1 {
            WalMode::Inline(self.replay)
        } else if self.config.parallel_skip_replay_wal {
            WalMode::Skipped
        } else {
            let depth = self.config.parallel_blocks;
            let (queue, mut wal_rx) =
                mpsc::channel::<(BlockNumber, Sealed<Arc<ReplayRecord>>, bool)>(depth);
            let (written_tx, written) = watch::channel(0u64);
            let replay = self.replay;
            tokio::spawn(async move {
                // Drain greedily: whatever queued while the previous group committed becomes ONE
                // multi-block commit (`write_many`), amortizing the RocksDB write — measured
                // ~1.6ms/block committed singly, the WAL writer's own throughput ceiling.
                const WAL_GROUP: usize = 32;
                let mut group = Vec::with_capacity(WAL_GROUP);
                loop {
                    group.clear();
                    if wal_rx.recv_many(&mut group, WAL_GROUP).await == 0 {
                        return; // queue closed: applier stopped
                    }
                    let last_block_number = group.last().expect("non-empty group").0;
                    let records = group
                        .drain(..)
                        .map(|(_, sealed, override_allowed)| (sealed, override_allowed))
                        .collect();
                    if let Err(err) = replay.write_many(records).await {
                        // Dropping `written_tx` errors the applier's `wait_for` -> shutdown.
                        tracing::error!("Failed to write replay records: {err}");
                        return;
                    }
                    written_tx.send_replace(last_block_number);
                }
            });
            WalMode::Pipelined { queue, written }
        };
        // A forwarded block's replay record must be durable first (no-op for Inline/Skipped).
        async fn wait_wal_written<R>(wal_mode: &mut WalMode<R>, block_number: BlockNumber) -> bool {
            match wal_mode {
                WalMode::Pipelined { written, .. } => written
                    .wait_for(|written| *written >= block_number)
                    .await
                    .is_ok(),
                WalMode::Inline(_) | WalMode::Skipped => true,
            }
        }
        // Sliding window: the cheap, order-sensitive work (replay write + state diff) stays
        // sequential as blocks stream in, but the expensive per-block repository `populate` (~25ms,
        // the bottleneck) is spawned without awaiting, so up to `parallel_blocks` populates run
        // concurrently across the round's disjoint blocks. The oldest is awaited + forwarded once the
        // window is full, preserving downstream block order. `parallel_blocks == 1` (production)
        // awaits each populate immediately -> unchanged one-at-a-time behaviour.
        let window = self.config.parallel_blocks.max(1);
        // (block_number, output, record, populate join handle), oldest at the front.
        let mut in_flight: std::collections::VecDeque<(
            u64,
            Arc<ReplayRecord>,
            Duration,
            Duration,
            tokio::task::JoinHandle<RepositoryResult<BlockOutputWithReads>>,
        )> = std::collections::VecDeque::with_capacity(window);
        // Outcome of waiting for work while populates are in flight: either the next block
        // arrives, or the oldest in-flight populate completes while the input is quiet.
        enum Next<P, R> {
            Payload(Option<P>),
            OldestPopulated(R),
        }
        loop {
            state_reporter.enter_state(BlockApplierState::Idle);
            let next = if in_flight.is_empty() {
                Next::Payload(input.recv_and_record_picked(&state_reporter).await)
            } else {
                // While populates are in flight, race the input against the oldest one: when
                // production goes quiet (e.g. the parallel producer parks on empty lanes at the
                // end of a load), the window drains instead of stranding up to `window - 1`
                // blocks — their receipts would otherwise never surface and receipt waiters
                // time out. `biased` + input-first keeps the sustained-load behaviour identical
                // (a ready block always wins; the full-window await below stays the
                // backpressure point). Racing `recv` is safe: tokio mpsc `recv` is cancel-safe.
                let oldest = &mut in_flight
                    .front_mut()
                    .expect("in_flight checked non-empty")
                    .4;
                tokio::select! {
                    biased;
                    payload = input.recv_and_record_picked(&state_reporter) => Next::Payload(payload),
                    res = &mut *oldest => Next::OldestPopulated(res),
                }
            };
            let maybe_payload = match next {
                Next::OldestPopulated(res) => {
                    let (bn, rec, add_state_elapsed, populate_spawn_elapsed, _handle) =
                        in_flight.pop_front().expect("in_flight checked non-empty");
                    let out = res.context("populate task panicked")??;
                    // Applied (state + repos visible) unblocks the executor's base-view sync
                    // immediately; only FORWARDING downstream waits for WAL durability, so the
                    // WAL writer's group-commit latency stays off the production round cadence.
                    self.applied_block_number_sender.send_replace(Some(bn));
                    if !wait_wal_written(&mut wal_mode, bn).await {
                        tracing::info!("Replay WAL writer stopped, shutting down");
                        return Ok(());
                    }
                    let t_output_send = Instant::now();
                    output.send_and_record(
                        AppliedBlock {
                            output: out,
                            record: Arc::try_unwrap(rec).unwrap_or_else(|arc| (*arc).clone()),
                        },
                        &state_reporter,
                    )?;
                    let output_send_elapsed = t_output_send.elapsed();
                    if parallel_producer_profile_enabled() {
                        tracing::info!(
                            block_number = bn,
                            in_flight_window = window,
                            ?add_state_elapsed,
                            ?populate_spawn_elapsed,
                            populate_wait_elapsed = ?Duration::ZERO,
                            ?output_send_elapsed,
                            "block_applier block profile"
                        );
                    } else if self.config.parallel_blocks > 1 {
                        tracing::info!(
                            block_number = bn,
                            ?add_state_elapsed,
                            ?populate_spawn_elapsed,
                            populate_wait_elapsed = ?Duration::ZERO,
                            ?output_send_elapsed,
                            "block_applier block done"
                        );
                    }
                    continue;
                }
                Next::Payload(p) => p,
            };
            let Some(BlockPayload {
                output: block_output_with_reads,
                record: executed_replay,
                command_type: cmd_type,
                failed_transactions,
            }) = maybe_payload
            else {
                // Channel closed: drain the in-flight window in order, then stop.
                while let Some((bn, rec, _, _, handle)) = in_flight.pop_front() {
                    let out = handle.await.context("populate task panicked")??;
                    self.applied_block_number_sender.send_replace(Some(bn));
                    if !wait_wal_written(&mut wal_mode, bn).await {
                        tracing::info!("Replay WAL writer stopped, shutting down");
                        return Ok(());
                    }
                    output.send_and_record(
                        AppliedBlock {
                            output: out,
                            record: Arc::try_unwrap(rec).unwrap_or_else(|arc| (*arc).clone()),
                        },
                        &state_reporter,
                    )?;
                }
                tracing::info!("inbound channel closed");
                return Ok(());
            };

            let block_output = block_output_with_reads.as_ref();
            // Arc-shared from here on: the WAL queue and the in-flight window both hold the
            // record, and a deep clone (~1,700 txs) on this serial loop is a measured
            // pipeline-visible cost. Unwrapped (or worst-case cloned) at the forward site.
            let executed_replay = Arc::new(executed_replay);
            let block_number = executed_replay.block_context.block_number;
            let block_hash = block_output.header.hash();
            let override_allowed = match cmd_type {
                BlockCommandType::Rebuild => true,
                _ if self.config.node_role.is_external() => true,
                _ => false,
            };

            state_reporter.enter_state(BlockApplierState::AddingToStorage);
            let t_wal = Instant::now();
            match &mut wal_mode {
                WalMode::Inline(replay) => {
                    if let Err(err) = replay
                        .write(
                            Sealed::new_unchecked((*executed_replay).clone(), block_hash),
                            override_allowed,
                        )
                        .await
                    {
                        tracing::info!("Failed to write replay record: {err}, shutting down");
                        return Ok(());
                    }
                }
                WalMode::Pipelined { queue, .. } => {
                    if queue
                        .send((
                            block_number,
                            Sealed::new_unchecked(executed_replay.clone(), block_hash),
                            override_allowed,
                        ))
                        .await
                        .is_err()
                    {
                        tracing::info!("Replay WAL writer stopped, shutting down");
                        return Ok(());
                    }
                }
                // Bench-only opt-in (`parallel_skip_replay_wal`): no restart / replay / proving
                // needed in the pure-throughput parallel benches.
                WalMode::Skipped => {}
            }
            let wal_write_elapsed = t_wal.elapsed();
            if parallel_producer_profile_enabled() {
                tracing::info!(
                    block_number,
                    txs = executed_replay.transactions.len(),
                    ?wal_write_elapsed,
                    "block_applier wal profile"
                );
            }

            // Sequential + in order: storage-diff contiguity requires ascending block numbers.
            let t_add_state = Instant::now();
            self.state.add_block_result(
                block_number,
                block_output.storage_writes.clone(),
                block_output
                    .published_preimages
                    .iter()
                    .map(|(k, v)| (*k, v)),
                override_allowed,
            )?;
            let add_state_elapsed = t_add_state.elapsed();

            // Spawn the expensive repository population (independent across disjoint blocks); do not
            // await it yet, so it overlaps with the next blocks' ingestion.
            let t_populate_spawn = Instant::now();
            let repositories = self.repositories.clone();
            let transactions = executed_replay.transactions.clone();
            let handle = tokio::spawn(async move {
                repositories
                    .populate(
                        block_output_with_reads.as_ref(),
                        transactions,
                        failed_transactions,
                    )
                    .await?;
                Ok(block_output_with_reads)
            });
            let populate_spawn_elapsed = t_populate_spawn.elapsed();
            in_flight.push_back((
                block_number,
                executed_replay,
                add_state_elapsed,
                populate_spawn_elapsed,
                handle,
            ));

            // Once `window` populates are in flight, await + forward the oldest (keeps order).
            if in_flight.len() >= window {
                state_reporter.enter_state(BlockApplierState::PopulatingRepos);
                let (bn, rec, add_state_elapsed, populate_spawn_elapsed, handle) =
                    in_flight.pop_front().unwrap();
                let t_populate_wait = Instant::now();
                let out = handle.await.context("populate task panicked")??;
                let populate_wait_elapsed = t_populate_wait.elapsed();
                self.applied_block_number_sender.send_replace(Some(bn));
                if !wait_wal_written(&mut wal_mode, bn).await {
                    tracing::info!("Replay WAL writer stopped, shutting down");
                    return Ok(());
                }
                let t_output_send = Instant::now();
                output.send_and_record(
                    AppliedBlock {
                        output: out,
                        record: Arc::try_unwrap(rec).unwrap_or_else(|arc| (*arc).clone()),
                    },
                    &state_reporter,
                )?;
                let output_send_elapsed = t_output_send.elapsed();
                if parallel_producer_profile_enabled() {
                    tracing::info!(
                        block_number = bn,
                        in_flight_window = window,
                        ?add_state_elapsed,
                        ?populate_spawn_elapsed,
                        ?populate_wait_elapsed,
                        ?output_send_elapsed,
                        "block_applier block profile"
                    );
                } else if self.config.parallel_blocks > 1 {
                    tracing::info!(
                        block_number = bn,
                        ?add_state_elapsed,
                        ?populate_spawn_elapsed,
                        ?populate_wait_elapsed,
                        ?output_send_elapsed,
                        "block_applier block done"
                    );
                }
            }
        }
    }
}
