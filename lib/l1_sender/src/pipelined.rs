//! Pipelined ("two-watermark") L1 sending.
//!
//! Submission, inclusion tracking, and confirmation run as concurrent tasks joined by
//! bounded queues, so new transactions go out while earlier ones are still waiting to be
//! mined or confirmed:
//!
//! ```text
//! submitter ──(submitted queue: entries hold an unmined-slot permit)──▶ inclusion watcher
//! inclusion watcher ──(mined queue)──▶ confirmation forwarder ──▶ downstream
//! inclusion watcher ──(watch: mined-nonce floor)──▶ submitter (sim-prefix pruning)
//! ```
//!
//! The two watermarks:
//!
//! * **Unmined window** — at most `command_limit` transactions may be submitted but not yet
//!   observed mined. This matches the L1 pool's *per-account* cap (geth blobpool
//!   `maxTxsPerAccount` = reth `max-account-slots` = 16), which counts pooled (unmined)
//!   transactions only. Each submission takes a semaphore permit that the inclusion watcher
//!   releases at the *first receipt sighting* — never at confirmation, which would keep each
//!   window slot occupied for `required_confirmations - 1` extra blocks per transaction.
//! * **Confirmation depth** — downstream forwarding still waits for `required_confirmations`,
//!   handled by a separate FIFO task so it delays forwarding by a constant without ever
//!   blocking submission.
//!
//! Invariants:
//!
//! * Nonces are assigned strictly sequentially by the single submitter; `next_nonce` advances
//!   only on a successful send. Any ambiguous send outcome (transport error) is fatal —
//!   restart + in-flight recovery is the only safe way to resolve it.
//! * Downstream forwarding is strict FIFO = nonce order = batch order (same-sender L1
//!   transactions are included in nonce order, so receipts appear in FIFO order too).
//! * Transient window overshoot (an L1 reorg can briefly return "mined" transactions to the
//!   pool after their permits were released) is absorbed by treating pool-capacity rejections
//!   as retryable with a generous budget.
//! * A transaction evicted from the L1 pool (fee spike, pool pressure) is detected by the
//!   inclusion watcher and resent at its nonce — waiting for its receipt would wedge the
//!   window forever. The old hash stays tracked in case the evicted original still mines.
//! * The L1 RPC endpoint is assumed to serve a single consistent view (one node, or sticky
//!   routing to one): startup recovery trusts a single `pending` nonce read. A backend that
//!   has not seen our pooled transactions makes the sender race its own earlier sends —
//!   still safe (single-use nonces + the contract's batch-chain check revert anything
//!   inconsistent), but each affected nonce costs wasted gas and a spurious restart.

use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderFeeConfig;
use crate::metrics::L1_SENDER_METRICS;
use crate::pipeline_component::L1Sender;
use crate::{FeeParams, L1SenderState, METHOD_NOT_FOUND_CODE, PreparedSidecar, SimPrefixEntry};
use alloy::consensus::Transaction as ConsensusTransaction;
use alloy::network::{Ethereum, Network, TransactionResponse};
use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::transports::TransportError;
use anyhow::Context as _;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_observability::ComponentStateReporter;
use zksync_os_pipeline::{PeekableReceiver, SendAndRecordExt};

type L1TxResponse = <Ethereum as Network>::TransactionResponse;

/// Sizing factor for the mined-but-unconfirmed queue. The physical population is bounded by
/// (inclusion rate × confirmation window); the bound only exists to cap memory if the
/// confirmation forwarder stalls, at which point permit starvation pauses submission.
const MINED_QUEUE_CAPACITY_FACTOR: usize = 4;

/// A transaction submitted to L1 but not yet observed mined. Holds one unmined-window permit,
/// released by the inclusion watcher at the first receipt sighting.
struct InFlightTx<Input> {
    /// Candidate hashes for this nonce, oldest first. Grows when a pool-evicted transaction
    /// is resent: the old hash stays a candidate because eviction is observed remotely and
    /// the original may still mine (e.g. it was propagated before the eviction).
    tx_hashes: Vec<B256>,
    nonce: u64,
    command: Input,
    submitted_at: Instant,
    permit: OwnedSemaphorePermit,
}

/// Inclusion-watcher → submitter request to resend the transaction at `nonce` (its previous
/// send was evicted from the L1 pool). The submitter rebuilds it from the nonce's
/// simulation-prefix entry and replies with the new hash.
struct ResendRequest {
    nonce: u64,
    reply: tokio::sync::oneshot::Sender<B256>,
}

/// A transaction observed mined, awaiting `required_confirmations` before its command is
/// forwarded downstream.
struct MinedTx<Input> {
    tx_hash: B256,
    nonce: u64,
    command: Input,
    submitted_at: Instant,
}

/// Fee floor applying while the submitter replaces a stale in-flight suffix left by a previous
/// session (recovery calldata mismatch or a dropped transaction). The floor is the stale
/// transactions' actual fees bumped by the pool's price-bump rule: 100% on all three fields
/// for blob transactions (geth and reth), 10% otherwise.
#[derive(Clone, Copy, Debug)]
struct ReplacementPlan {
    /// Nonces below this replace previously-submitted transactions.
    until_nonce: u64,
    fee_floor: FeeParams,
}

/// A previous session's in-flight transaction paired with its queued command during recovery.
struct RecoveredInFlight<Input> {
    tx_hash: B256,
    nonce: u64,
    command: Input,
    /// Fees the transaction was actually sent with — reused for its simulation-prefix entry.
    fee_params: FeeParams,
    /// Gas limit the transaction was sent with — reused if it must be resent after eviction.
    gas_limit: u64,
}

/// Startup state for the pipelined sender, derived from in-flight transaction recovery.
struct PipelinedStart<Input> {
    /// Already-submitted transactions to track (nonce order). Their commands were consumed
    /// from the inbound queue.
    seeds: Vec<RecoveredInFlight<Input>>,
    /// Nonce of the next new submission.
    next_nonce: u64,
    /// All nonces below this are known mined (confirmed-nonce baseline at startup).
    mined_floor: u64,
    replacement: Option<ReplacementPlan>,
}

/// Mutable submitter-side state threaded through send waves.
struct SubmitterState {
    next_nonce: u64,
    /// Submitted-but-not-observed-mined transactions mirrored into `eth_simulateV1` payloads
    /// (see [`SimPrefixEntry`]). Pruned against the inclusion watcher's mined-nonce floor.
    in_flight_prefix: VecDeque<SimPrefixEntry>,
    replacement: Option<ReplacementPlan>,
}

impl<Input> L1Sender<Input>
where
    Input: SendToL1 + Send + 'static,
{
    pub(crate) async fn run_pipelined(
        &self,
        mut inbound: PeekableReceiver<L1SenderCommand<Input>>,
        outbound: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
        state_reporter: ComponentStateReporter,
        latest_nonce: u64,
    ) -> anyhow::Result<()> {
        let command_name = Input::COMPONENT_ID.as_str();
        let operator_address = self.operator_address().await?;
        let window = self.config.command_limit.max(1);

        let start = self
            .plan_pipelined_recovery(
                &mut inbound,
                &state_reporter,
                operator_address,
                latest_nonce,
            )
            .await?
            .context("inbound channel closed during in-flight recovery")?;

        tracing::info!(
            command_name,
            window,
            seeds = start.seeds.len(),
            next_nonce = start.next_nonce,
            replacement = ?start.replacement,
            "starting pipelined L1 sender",
        );

        let slots = Arc::new(Semaphore::new(window));
        // Entries hold an unmined-slot permit, so occupancy never exceeds `window`.
        let (submitted_tx, mut submitted_rx) = mpsc::channel::<InFlightTx<Input>>(window);
        let (mined_tx, mut mined_rx) =
            mpsc::channel::<MinedTx<Input>>(MINED_QUEUE_CAPACITY_FACTOR * window + 64);
        // "All nonces below this value have been observed mined."
        let (mined_floor_tx, mined_floor_rx) = watch::channel(start.mined_floor);
        // Inclusion watcher asks the submitter to resend pool-evicted transactions. The
        // watcher requests resends one at a time (head of line), so capacity 1 suffices.
        let (resend_tx, mut resend_rx) = mpsc::channel::<ResendRequest>(1);

        let state_reporter = &state_reporter;

        let submitter = {
            let slots = Arc::clone(&slots);
            let submitted_tx = submitted_tx;
            async move {
                let mut state = SubmitterState {
                    next_nonce: start.next_nonce,
                    in_flight_prefix: VecDeque::new(),
                    replacement: start.replacement,
                };
                self.seed_recovered_txs(
                    start.seeds,
                    &mut state,
                    &slots,
                    &submitted_tx,
                    operator_address,
                )
                .await?;

                let mut raw_buffer: Vec<L1SenderCommand<Input>> = Vec::with_capacity(window);
                loop {
                    // Wait for window capacity first, then for commands: idle permits are
                    // never contended, and this order keeps commands in the channel (visible
                    // to backpressure) until they can actually be sent. Both waits service
                    // resend requests: the watcher may need a resend precisely when the
                    // window is full and no new sends can proceed.
                    state_reporter.enter_state(L1SenderState::WaitingL1Inclusion);
                    let first_permit = loop {
                        tokio::select! {
                            biased;
                            Some(request) = resend_rx.recv() => {
                                self.resend_evicted(&mut state, request, operator_address).await?;
                            }
                            permit = Arc::clone(&slots).acquire_owned() => {
                                break permit.expect("in-flight window semaphore is never closed");
                            }
                        }
                    };
                    let mut permits = vec![first_permit];
                    while permits.len() < window {
                        match Arc::clone(&slots).try_acquire_owned() {
                            Ok(permit) => permits.push(permit),
                            Err(_) => break,
                        }
                    }

                    state_reporter.enter_state(L1SenderState::Idle);
                    let received = loop {
                        tokio::select! {
                            biased;
                            Some(request) = resend_rx.recv() => {
                                self.resend_evicted(&mut state, request, operator_address).await?;
                            }
                            received = inbound.recv_many(&mut raw_buffer, permits.len()) => {
                                break received;
                            }
                        }
                    };
                    if received == 0 {
                        tracing::info!(command_name, "inbound channel closed");
                        // Drop `submitted_tx` (by returning) so the inclusion watcher and the
                        // confirmation forwarder drain their queues and exit.
                        return anyhow::Ok(());
                    }
                    // Excess permits are released by dropping them.
                    permits.truncate(received);

                    let last = raw_buffer
                        .last()
                        .context("recv_many returned non-zero count but the buffer is empty")?;
                    state_reporter.record_picked(
                        last.last_block_number(),
                        last.block_timestamp(),
                        Some(last.last_batch_number()),
                    );
                    let commands = raw_buffer
                        .drain(..)
                        .map(|cmd| -> anyhow::Result<Input> {
                            match cmd {
                                L1SenderCommand::SendToL1(command) => Ok(command),
                                L1SenderCommand::Passthrough(batch) => anyhow::bail!(
                                    "Unexpected passthrough command for batch {:?}. \
                                     No passthrough commands are expected after the first `SendToL1`.",
                                    batch.batch_number()
                                ),
                            }
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;

                    state_reporter.enter_state(L1SenderState::SendingToL1);
                    self.submit_wave(
                        &mut state,
                        commands,
                        permits,
                        operator_address,
                        &submitted_tx,
                        &mined_floor_rx,
                    )
                    .await?;
                    L1_SENDER_METRICS.txs_in_flight[&command_name]
                        .set((window - slots.available_permits()) as u64);
                }
            }
        };

        let inclusion_watcher = {
            let slots = Arc::clone(&slots);
            let mined_tx = mined_tx;
            let resend_tx = resend_tx;
            async move {
                while let Some(mut entry) = submitted_rx.recv().await {
                    // Same-sender transactions mine in nonce order, so head-of-line sighting
                    // is exhaustive: this receipt appearing implies nothing behind it is
                    // mined-but-unobserved for long.
                    let mined_hash = self
                        .wait_for_inclusion(&mut entry, operator_address, &resend_tx)
                        .await?;
                    let InFlightTx {
                        nonce,
                        command,
                        submitted_at,
                        permit,
                        ..
                    } = entry;
                    // First sighting frees an unmined-window slot; confirmation depth is the
                    // forwarder's concern only.
                    drop(permit);
                    mined_floor_tx.send_replace(nonce + 1);
                    L1_SENDER_METRICS.txs_in_flight[&command_name]
                        .set((window - slots.available_permits()) as u64);
                    if mined_tx
                        .send(MinedTx {
                            tx_hash: mined_hash,
                            nonce,
                            command,
                            submitted_at,
                        })
                        .await
                        .is_err()
                    {
                        // The forwarder only stops on error; `try_join!` surfaces its error,
                        // this just terminates the watcher promptly.
                        anyhow::bail!("confirmation forwarder terminated");
                    }
                }
                anyhow::Ok(())
            }
        };

        let confirmation_forwarder = async move {
            while let Some(mined) = mined_rx.recv().await {
                let receipt = self.wait_for_confirmed_receipt(mined.tx_hash).await;
                // Observe latency before propagating errors so timeout cases are recorded.
                L1_SENDER_METRICS.tx_inclusion_latency_seconds[&command_name]
                    .observe(mined.submitted_at.elapsed().as_secs_f64());
                let receipt = receipt?;
                self.validate_tx_receipt(&mined.command, receipt).await?;
                tracing::info!(
                    command_name,
                    nonce = mined.nonce,
                    tx_hash = ?mined.tx_hash,
                    "L1 transaction confirmed, sending downstream",
                );
                for mut envelope in mined.command.into() {
                    envelope.set_stage(Input::MINED_STAGE);
                    outbound.send_and_record(envelope, state_reporter)?;
                }
            }
            anyhow::Ok(())
        };

        // Boxed for stack safety, not style: these three state machines (the submitter
        // embeds the whole wave-submission and recovery-seeding machinery) otherwise live
        // inline in this future, which itself is moved by value when the pipeline spawns the
        // component — that spike overflowed an 8 MiB thread stack in debug builds.
        tokio::try_join!(
            Box::pin(submitter),
            Box::pin(inclusion_watcher),
            Box::pin(confirmation_forwarder)
        )?;
        Ok(())
    }

    /// Hands a previous session's recovered in-flight transactions to the tracking tasks.
    /// Each seed occupies an unmined-window permit (released as usual once observed mined) and
    /// gets a simulation-prefix entry so gas estimation for new commands chains correctly.
    async fn seed_recovered_txs(
        &self,
        seeds: Vec<RecoveredInFlight<Input>>,
        state: &mut SubmitterState,
        slots: &Arc<Semaphore>,
        submitted_tx: &mpsc::Sender<InFlightTx<Input>>,
        operator_address: Address,
    ) -> anyhow::Result<()> {
        let use_eip7594 = self.provider.capabilities().supports_eip7594;
        for seed in seeds {
            let sidecar = match seed.command.blob_sidecar() {
                Some(sidecar) => Some(Arc::new(
                    PreparedSidecar::prepare(sidecar, use_eip7594).await?,
                )),
                None => None,
            };
            state.in_flight_prefix.push_back(SimPrefixEntry {
                nonce: seed.nonce,
                calldata: seed.command.solidity_call(&operator_address),
                sidecar,
                fee_params: seed.fee_params,
                gas_limit: seed.gas_limit,
            });
            self.note_submitted_batches(&seed.command);
            // If there are more seeds than window slots (the window was larger in a previous
            // session), this blocks until the inclusion watcher frees permits — it is already
            // running and needs no permits itself.
            let permit = Arc::clone(slots)
                .acquire_owned()
                .await
                .expect("in-flight window semaphore is never closed");
            submitted_tx
                .send(InFlightTx {
                    tx_hashes: vec![seed.tx_hash],
                    nonce: seed.nonce,
                    command: seed.command,
                    submitted_at: Instant::now(),
                    permit,
                })
                .await
                .map_err(|_| anyhow::anyhow!("inclusion watcher terminated during seeding"))?;
        }
        Ok(())
    }

    /// Sends one wave of commands (bounded by the permits handed in), assigning sequential
    /// nonces and pushing each transaction to the inclusion watcher.
    async fn submit_wave(
        &self,
        state: &mut SubmitterState,
        commands: Vec<Input>,
        mut permits: Vec<OwnedSemaphorePermit>,
        operator_address: Address,
        submitted_tx: &mpsc::Sender<InFlightTx<Input>>,
        mined_floor_rx: &watch::Receiver<u64>,
    ) -> anyhow::Result<()> {
        let command_name = Input::COMPONENT_ID.as_str();
        let range = Input::display_range(&commands);
        tracing::info!(command_name, range, "sending L1 transactions");
        L1_SENDER_METRICS.parallel_transactions[&command_name].set(commands.len() as u64);

        // Drop prefix entries for transactions the inclusion watcher has observed mined —
        // their state is part of the provider's simulation base now.
        let mined_floor = *mined_floor_rx.borrow();
        while state
            .in_flight_prefix
            .front()
            .is_some_and(|entry| entry.nonce < mined_floor)
        {
            state.in_flight_prefix.pop_front();
        }

        let fee_params = self
            .resolve_fee_params(
                self.config.fee_config,
                self.config.force_transaction_resubmission,
            )
            .await?;
        let fee_params = self.apply_replacement_floor(state, fee_params);

        let prepared_sidecars = self.prepare_sidecars(&commands).await?;
        let gas_limits = self
            .estimate_gas_limits(
                state.in_flight_prefix.make_contiguous(),
                &commands,
                &prepared_sidecars,
                operator_address,
                fee_params,
                state.next_nonce,
            )
            .await?;
        tracing::info!(
            command_name,
            range,
            ?gas_limits,
            "estimated gas limits via eth_simulateV1",
        );
        let blob_base_fee = self
            .fetch_blob_base_fee_if_needed(&prepared_sidecars)
            .await?;

        for ((mut command, gas_limit), prepared_sidecar) in
            commands.into_iter().zip(gas_limits).zip(prepared_sidecars)
        {
            let nonce = state.next_nonce;
            let calldata = command.solidity_call(&operator_address);
            let tx_request = self.build_tx_request(
                calldata.clone(),
                operator_address,
                nonce,
                gas_limit,
                fee_params,
                prepared_sidecar.as_deref(),
                blob_base_fee,
            );

            // Notify CommitWatcher before the transaction can possibly land on L1:
            // this batch number is being submitted by this session.
            self.note_submitted_batches(&command);

            let pending_tx = self.send_tx_with_retries(tx_request, &range).await?;
            let tx_hash = *pending_tx.tx_hash();
            state.next_nonce += 1;
            tracing::info!(
                "{command_name}: L1 transaction submitted for {range}. Hash: {tx_hash:?} (nonce {nonce})",
            );

            command
                .as_mut()
                .iter_mut()
                .for_each(|envelope| envelope.set_stage(Input::SENT_STAGE));
            state.in_flight_prefix.push_back(SimPrefixEntry {
                nonce,
                calldata,
                sidecar: prepared_sidecar,
                fee_params,
                gas_limit,
            });

            let permit = permits
                .pop()
                .context("fewer permits than commands in a wave")?;
            submitted_tx
                .send(InFlightTx {
                    tx_hashes: vec![tx_hash],
                    nonce,
                    command,
                    submitted_at: Instant::now(),
                    permit,
                })
                .await
                .map_err(|_| anyhow::anyhow!("inclusion watcher terminated"))?;
        }
        Ok(())
    }

    /// Applies (and expires) the recovery replacement-fee floor for the current wave. If any
    /// nonce in the wave replaces a stale transaction, the whole wave gets the floor — a
    /// bounded overpay that keeps fee resolution per-wave instead of per-tx.
    fn apply_replacement_floor(
        &self,
        state: &mut SubmitterState,
        fee_params: FeeParams,
    ) -> FeeParams {
        match state.replacement {
            Some(plan) if state.next_nonce < plan.until_nonce => {
                let raised = fee_params.max(plan.fee_floor);
                tracing::warn!(
                    command_name = Input::COMPONENT_ID.as_str(),
                    until_nonce = plan.until_nonce,
                    next_nonce = state.next_nonce,
                    ?raised,
                    "replacing stale in-flight transactions from a previous session; \
                     applying a 2x replacement fee floor (may exceed configured fee caps)",
                );
                raised
            }
            Some(_) => {
                state.replacement = None;
                fee_params
            }
            None => fee_params,
        }
    }

    /// Polls until one of the entry's candidate transactions has a receipt (any depth) and
    /// returns that hash. Never gives up on a transaction that is merely unmined — but a
    /// transaction *evicted from the pool* (fee spike, pool pressure) would otherwise be
    /// waited on forever, wedging the whole window, so eviction is detected and the
    /// transaction is resent at its nonce via the submitter.
    async fn wait_for_inclusion(
        &self,
        entry: &mut InFlightTx<Input>,
        operator_address: Address,
        resend_tx: &mpsc::Sender<ResendRequest>,
    ) -> anyhow::Result<B256> {
        /// Pool presence is probed every N-th empty receipt poll (it costs one RPC per
        /// candidate), and eviction is acted on only after two consecutive absent probes —
        /// a just-submitted transaction can be transiently invisible.
        const EVICTION_PROBE_EVERY: u32 = 5;
        const EVICTION_PROBES_TO_CONFIRM: u32 = 2;

        let timeout = self.config.transaction_timeout;
        let poll_interval = self.config.poll_interval;
        let started_at = Instant::now();
        let mut next_warning_at = if timeout.is_zero() {
            None
        } else {
            Some(timeout)
        };
        let mut polls: u32 = 0;
        let mut consecutive_absent: u32 = 0;
        loop {
            for &tx_hash in &entry.tx_hashes {
                let receipt = self
                    .provider
                    .get_transaction_receipt(tx_hash)
                    .await
                    .with_context(|| {
                        format!("fetch receipt while waiting for L1 inclusion of tx {tx_hash}")
                    })?;
                if receipt.is_some() {
                    return Ok(tx_hash);
                }
            }

            polls += 1;
            if polls.is_multiple_of(EVICTION_PROBE_EVERY) {
                if self.all_candidates_absent(entry).await? {
                    consecutive_absent += 1;
                } else {
                    consecutive_absent = 0;
                }
                if consecutive_absent >= EVICTION_PROBES_TO_CONFIRM {
                    self.handle_evicted_entry(entry, operator_address, resend_tx)
                        .await?;
                    consecutive_absent = 0;
                }
            }

            let elapsed = started_at.elapsed();
            if let Some(warning_at) = next_warning_at
                && elapsed >= warning_at
            {
                tracing::warn!(
                    command_name = Input::COMPONENT_ID.as_str(),
                    tx_hashes = ?entry.tx_hashes,
                    nonce = entry.nonce,
                    waited_secs = elapsed.as_secs_f64(),
                    "still waiting for L1 inclusion; if the transaction is stuck underpriced, \
                     restart with `force_transaction_resubmission.enabled = true`",
                );
                next_warning_at = Some(warning_at + timeout);
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    /// True when none of the entry's candidate transactions are known to the L1 node —
    /// neither pooled nor mined.
    async fn all_candidates_absent(&self, entry: &InFlightTx<Input>) -> anyhow::Result<bool> {
        for &tx_hash in &entry.tx_hashes {
            let known = self
                .provider
                .get_transaction_by_hash(tx_hash)
                .await
                .with_context(|| format!("probe pool presence of tx {tx_hash}"))?
                .is_some();
            if known {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// All candidates for this nonce vanished from the pool. If the nonce was consumed by a
    /// transaction that is none of ours, another sender is using the operator account — fatal.
    /// Otherwise the transaction was evicted (fee spike, pool pressure): ask the submitter to
    /// resend it at the same nonce and track the new hash alongside the old candidates (the
    /// eviction is observed remotely, so an old candidate may still mine elsewhere).
    async fn handle_evicted_entry(
        &self,
        entry: &mut InFlightTx<Input>,
        operator_address: Address,
        resend_tx: &mpsc::Sender<ResendRequest>,
    ) -> anyhow::Result<()> {
        let latest_nonce = self
            .provider
            .get_transaction_count(operator_address)
            .await
            .context("get confirmed nonce while handling a pool eviction")?;
        if latest_nonce > entry.nonce {
            // Give in-flight receipt propagation one more chance before declaring foul play.
            for &tx_hash in &entry.tx_hashes {
                if self
                    .provider
                    .get_transaction_receipt(tx_hash)
                    .await?
                    .is_some()
                {
                    return Ok(());
                }
            }
            anyhow::bail!(
                "nonce {} was consumed by a transaction that is not one of ours ({:?}); \
                 the operator account is being used by another sender",
                entry.nonce,
                entry.tx_hashes,
            );
        }

        tracing::warn!(
            command_name = Input::COMPONENT_ID.as_str(),
            nonce = entry.nonce,
            tx_hashes = ?entry.tx_hashes,
            "in-flight transaction was evicted from the L1 pool; resending at the same nonce",
        );
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        resend_tx
            .send(ResendRequest {
                nonce: entry.nonce,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("submitter terminated during an eviction resend"))?;
        let new_hash = reply_rx
            .await
            .context("submitter dropped an eviction resend request")?;
        entry.tx_hashes.push(new_hash);
        entry.submitted_at = Instant::now();
        Ok(())
    }

    /// Rebuilds and resends the transaction at `request.nonce` from its simulation-prefix
    /// entry after a pool eviction. Fees are re-resolved and floored at the original send's
    /// fees plus the pool's replacement bump, in case the evicted transaction resurfaces.
    async fn resend_evicted(
        &self,
        state: &mut SubmitterState,
        request: ResendRequest,
        operator_address: Address,
    ) -> anyhow::Result<()> {
        let entry = state
            .in_flight_prefix
            .iter_mut()
            .find(|entry| entry.nonce == request.nonce)
            .with_context(|| {
                format!(
                    "no in-flight prefix entry for evicted nonce {}",
                    request.nonce
                )
            })?;

        let resolved = self
            .resolve_fee_params(
                self.config.fee_config,
                self.config.force_transaction_resubmission,
            )
            .await?;
        let fee_params = resolved.max(entry.fee_params.replacement_floor(entry.sidecar.is_some()));

        let blob_base_fee = if entry.sidecar.is_some() {
            Some(self.provider.get_blob_base_fee().await?)
        } else {
            None
        };
        let tx_request = self.build_tx_request(
            entry.calldata.clone(),
            operator_address,
            entry.nonce,
            entry.gas_limit,
            fee_params,
            entry.sidecar.as_deref(),
            blob_base_fee,
        );
        let pending_tx = self
            .send_tx_with_retries(tx_request, &format!("resend of nonce {}", request.nonce))
            .await?;
        entry.fee_params = fee_params;

        let new_hash = *pending_tx.tx_hash();
        tracing::info!(
            command_name = Input::COMPONENT_ID.as_str(),
            nonce = request.nonce,
            tx_hash = ?new_hash,
            "resent evicted L1 transaction",
        );
        // The watcher owns the entry lifecycle; if it is gone the try_join surfaces its error.
        let _ = request.reply.send(new_hash);
        Ok(())
    }

    /// Detects in-flight L1 transactions from a previous session and derives the pipelined
    /// startup state: which transactions to track as already-submitted, which nonce to
    /// continue from, and whether a stale suffix must be replaced with bumped fees.
    ///
    /// Pairing: for each pending nonce the on-chain transaction's calldata is compared
    /// against the next queued command (pinned to the same L1 block at which the inbound
    /// queue was constructed — see `l1_block_number`). A match is tracked as
    /// already-submitted; a mismatch or dropped transaction produces a [`ReplacementPlan`] so
    /// the stale suffix is replaced at its own nonces instead of appending duplicates behind
    /// it.
    ///
    /// Returns `None` when the inbound channel closes mid-recovery.
    async fn plan_pipelined_recovery(
        &self,
        inbound: &mut PeekableReceiver<L1SenderCommand<Input>>,
        state_reporter: &ComponentStateReporter,
        operator_address: Address,
        latest_nonce: u64,
    ) -> anyhow::Result<Option<PipelinedStart<Input>>> {
        let command_name = Input::COMPONENT_ID.as_str();
        let pending_nonce = self
            .provider
            .get_transaction_count(operator_address)
            .pending()
            .await
            .context("get pending transaction count")?;

        if self.config.force_transaction_resubmission {
            if pending_nonce > latest_nonce {
                tracing::warn!(
                    command_name,
                    latest_nonce,
                    pending_nonce,
                    "force resubmission: replacing {} in-flight transactions starting from the \
                     confirmed nonce with replacement fees",
                    pending_nonce - latest_nonce,
                );
            }
            // Replacement fees are applied session-wide via `resolve_fee_params`; config
            // validation guarantees the multipliers satisfy the 100% blob-RBF bump rule.
            return Ok(Some(PipelinedStart {
                seeds: vec![],
                next_nonce: latest_nonce,
                mined_floor: latest_nonce,
                replacement: None,
            }));
        }

        if pending_nonce <= latest_nonce {
            return Ok(Some(PipelinedStart {
                seeds: vec![],
                next_nonce: pending_nonce.max(latest_nonce),
                mined_floor: latest_nonce,
                replacement: None,
            }));
        }

        let in_flight_count = (pending_nonce - latest_nonce) as usize;
        tracing::info!(
            command_name,
            l1_block_number = self.l1_block_number,
            latest_nonce,
            pending_nonce,
            in_flight_count,
            "Detected in-flight L1 transactions on startup, attempting recovery",
        );

        // Probe whether the provider supports `eth_getTransactionBySenderAndNonce` before
        // iterating over all pending nonces.
        if let Err(TransportError::ErrorResp(ref e)) = self
            .provider
            .get_transaction_by_sender_nonce(operator_address, latest_nonce)
            .await
        {
            if e.code == METHOD_NOT_FOUND_CODE {
                tracing::warn!(
                    command_name,
                    in_flight_count,
                    "eth_getTransactionBySenderAndNonce is not supported by the current L1 \
                     provider, so in-flight transactions cannot be paired with queued commands. \
                     Waiting for them to settle before sending anything. For crash-tolerant \
                     operation use a provider that supports the method (reth/Erigon family), or \
                     restart with `force_transaction_resubmission.enabled = true` to replace \
                     them.",
                );
                let settled_nonce = self
                    .wait_for_in_flight_txs_to_settle(operator_address)
                    .await?;
                return Ok(Some(PipelinedStart {
                    seeds: vec![],
                    next_nonce: settled_nonce,
                    mined_floor: settled_nonce,
                    replacement: None,
                }));
            }
            anyhow::bail!("Error while probing eth_getTransactionBySenderAndNonce support: {e}");
        }

        let mut seeds: Vec<RecoveredInFlight<Input>> = Vec::with_capacity(in_flight_count);
        let mut replacement = None;
        for nonce in latest_nonce..pending_nonce {
            let tx = match self
                .provider
                .get_transaction_by_sender_nonce(operator_address, nonce)
                .await
            {
                Err(err) => {
                    anyhow::bail!("Failed to fetch in-flight transaction at nonce {nonce}: {err}");
                }
                Ok(Some(tx)) => tx,
                Ok(None) => {
                    tracing::warn!(
                        command_name,
                        nonce,
                        "In-flight transaction at nonce {nonce} was dropped from the mempool; \
                         re-sending queued commands from that nonce",
                    );
                    replacement = self
                        .build_replacement_plan(operator_address, nonce, pending_nonce, None)
                        .await;
                    break;
                }
            };

            // Peek at the next command without consuming it so that a mismatch leaves
            // `inbound` intact for the normal send path.
            let matches = inbound
                .peek_recv(|raw_cmd| {
                    let L1SenderCommand::SendToL1(cmd) = raw_cmd else {
                        return false;
                    };
                    cmd.solidity_call(&operator_address) == *tx.input()
                })
                .await;

            match matches {
                None => return Ok(None),
                Some(false) => {
                    tracing::warn!(
                        command_name,
                        nonce,
                        "In-flight transaction calldata does not match the next queued command; \
                         replacing in-flight transactions from nonce {nonce} with queued \
                         commands at bumped fees",
                    );
                    replacement = self
                        .build_replacement_plan(operator_address, nonce, pending_nonce, Some(&tx))
                        .await;
                    break;
                }
                Some(true) => {
                    let Some(L1SenderCommand::SendToL1(cmd)) =
                        inbound.recv_and_record_picked(state_reporter).await
                    else {
                        unreachable!("peek succeeded, recv must return the same item");
                    };
                    seeds.push(RecoveredInFlight {
                        tx_hash: tx.tx_hash(),
                        nonce,
                        command: cmd,
                        fee_params: fee_params_of_tx(&tx, self.config.fee_config),
                        gas_limit: ConsensusTransaction::gas_limit(&tx),
                    });
                }
            }
        }

        let next_nonce = latest_nonce + seeds.len() as u64;
        tracing::info!(
            command_name,
            recovered = seeds.len(),
            in_flight_count,
            next_nonce,
            replacement_active = replacement.is_some(),
            "Recovered in-flight transactions; tracking their inclusion in the background",
        );

        Ok(Some(PipelinedStart {
            seeds,
            next_nonce,
            mined_floor: latest_nonce,
            replacement,
        }))
    }

    /// Computes the replacement-fee floor for a stale in-flight suffix `[from_nonce,
    /// until_nonce)`: the per-field maximum of every still-visible stale transaction, bumped
    /// by the pool's price-bump rule — 100% when any stale transaction carries blobs, 10%
    /// otherwise. Returns `None` when no stale transaction is visible anymore (nothing to
    /// outbid).
    async fn build_replacement_plan(
        &self,
        operator_address: Address,
        from_nonce: u64,
        until_nonce: u64,
        first_stale: Option<&L1TxResponse>,
    ) -> Option<ReplacementPlan> {
        let mut stale_fee_max: Option<FeeParams> = None;
        let mut any_blobs = false;
        let mut fold = |tx: &L1TxResponse| {
            let fees = fee_params_of_tx(tx, self.config.fee_config);
            any_blobs |= ConsensusTransaction::max_fee_per_blob_gas(tx).is_some();
            stale_fee_max = Some(match stale_fee_max {
                Some(current) => current.max(fees),
                None => fees,
            });
        };
        if let Some(tx) = first_stale {
            fold(tx);
        }
        let already_fetched = first_stale.is_some() as u64;
        for nonce in (from_nonce + already_fetched)..until_nonce {
            match self
                .provider
                .get_transaction_by_sender_nonce(operator_address, nonce)
                .await
            {
                Ok(Some(tx)) => fold(&tx),
                // A dropped tx needs no outbidding; an error here must not block startup —
                // worst case the floor is too low and the send fails fatally with a clear
                // "replacement transaction underpriced" error.
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        nonce,
                        %err,
                        "failed to fetch stale in-flight transaction while computing the \
                         replacement fee floor",
                    );
                }
            }
        }

        stale_fee_max.map(|fees| ReplacementPlan {
            until_nonce,
            fee_floor: fees.replacement_floor(any_blobs),
        })
    }

    /// Polls until the operator account has no pending transactions (pending nonce == latest
    /// nonce). Used when in-flight transactions exist but cannot be paired with queued
    /// commands because the provider lacks `eth_getTransactionBySenderAndNonce`.
    async fn wait_for_in_flight_txs_to_settle(
        &self,
        operator_address: Address,
    ) -> anyhow::Result<u64> {
        let timeout = self.config.transaction_timeout;
        let poll_interval = self.config.poll_interval;
        let started_at = Instant::now();
        let mut next_warning_at = if timeout.is_zero() {
            None
        } else {
            Some(timeout)
        };
        loop {
            let latest_nonce = self
                .provider
                .get_transaction_count(operator_address)
                .await
                .context(
                    "get latest transaction count while waiting for in-flight txs to settle",
                )?;
            let pending_nonce = self
                .provider
                .get_transaction_count(operator_address)
                .pending()
                .await
                .context(
                    "get pending transaction count while waiting for in-flight txs to settle",
                )?;
            if pending_nonce <= latest_nonce {
                return Ok(latest_nonce);
            }

            let elapsed = started_at.elapsed();
            if let Some(warning_at) = next_warning_at
                && elapsed >= warning_at
            {
                tracing::warn!(
                    command_name = Input::COMPONENT_ID.as_str(),
                    latest_nonce,
                    pending_nonce,
                    waited_secs = elapsed.as_secs_f64(),
                    "still waiting for previous-session in-flight transactions to settle; \
                     restart with `force_transaction_resubmission.enabled = true` to replace \
                     them instead",
                );
                next_warning_at = Some(warning_at + timeout);
            }
            tokio::time::sleep(poll_interval).await;
        }
    }
}

/// Extracts the fee parameters a transaction was actually sent with. The blob fee cap falls
/// back to the configured cap for non-blob transactions (the value is then unused).
fn fee_params_of_tx(tx: &L1TxResponse, fee_config: L1SenderFeeConfig) -> FeeParams {
    FeeParams {
        max_fee_per_gas: ConsensusTransaction::max_fee_per_gas(tx),
        max_priority_fee_per_gas: ConsensusTransaction::max_priority_fee_per_gas(tx)
            .unwrap_or_default(),
        max_fee_per_blob_gas: ConsensusTransaction::max_fee_per_blob_gas(tx)
            .unwrap_or(fee_config.max_fee_per_blob_gas_wei),
    }
}
