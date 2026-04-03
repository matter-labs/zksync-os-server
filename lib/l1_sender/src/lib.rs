pub mod batcher_metrics;
pub mod batcher_model;
pub mod commands;
pub mod config;
pub mod error;
mod metrics;
pub mod pipeline_component;
pub mod upgrade_gatekeeper;

pub use error::{L1SendError, RecoverableReason};

use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use alloy::consensus::BlobTransactionValidationError;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, B256};
use alloy::providers::ext::DebugApi;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{PendingTransactionError, Provider, WalletProvider, WatchTxError};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use futures::FutureExt;
use futures::future::BoxFuture;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use zksync_os_observability::{ComponentStateHandle, ComponentStateReporter};
use zksync_os_operator_signer::SignerConfig;
use zksync_os_pipeline::PeekableReceiver;

/// Maximum time to wait for a transaction to be included on L1.
///
/// Normally 15-30 seconds is enough for normal priority transactions, and 60-120 is enough for
/// lower gas price transactions. We picked 300 seconds conservatively as it should cover most
/// scenarios with network congestion.
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(300);

/// Future that resolves into a (fallible) transaction receipt.
type TransactionReceiptFuture =
    BoxFuture<'static, Result<TransactionReceipt, PendingTransactionError>>;

// ==============================================================================
// Exponential Backoff
// ==============================================================================

/// Simple exponential backoff with a configurable initial delay, multiplier, and cap.
///
/// Used to pace retries after transient and recoverable errors so we don't hammer
/// a struggling RPC endpoint.
struct ExponentialBackoff {
    initial: Duration,
    current: Duration,
    max: Duration,
}

impl ExponentialBackoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            current: initial,
            max,
        }
    }

    /// Returns the current delay and doubles it (capped at `max`) for the next call.
    fn next(&mut self) -> Duration {
        let delay = self.current;
        self.current = std::cmp::min(self.current * 2, self.max);
        delay
    }

    /// Resets the delay to the initial value after a successful cycle.
    fn reset(&mut self) {
        self.current = self.initial;
    }
}

// ==============================================================================
// InFlightTx
// ==============================================================================

/// A transaction that has been submitted to L1 but not yet confirmed.
///
/// We track `tx_hash` separately from the receipt future so that if the future
/// times out (and is consumed), we can log the hash and re-queue the command.
struct InFlightTx<Input> {
    command: Input,
    tx_hash: B256,
    receipt_future: TransactionReceiptFuture,
}

// ==============================================================================
// L1SenderLoop
// ==============================================================================

/// Phase-based state machine for the L1 send loop.
///
/// Commands flow through three collections on their way to L1:
///
///   inbound channel
///       → `pending_commands`  (received, not yet sent)
///       → `in_flight`         (sent to L1, awaiting receipt)
///       → `completed`         (receipt received, not yet forwarded)
///       → outbound channel
///
/// Each field survives errors: a transient RPC failure during `send_pending`
/// leaves already-sent commands in `in_flight` and unsent ones in
/// `pending_commands`. On retry we only send what hasn't been sent yet.
///
/// All three senders (commit, prove, execute) share this generic code.
/// `Input: SendToL1` parameterises the command-specific behavior.
struct L1SenderLoop<Input, F, P>
where
    Input: SendToL1,
    F: TxFiller<Ethereum>,
    P: Provider<Ethereum>,
{
    // Plumbing
    inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: Sender<SignedBatchEnvelope<FriProof>>,
    provider: FillProvider<F, P>,
    config: L1SenderConfig<Input>,
    to_address: Address,
    operator_address: Address,
    gateway: bool,

    // Per-cycle state — all three collections must be empty at the start of receive()
    pending_commands: Vec<Input>,
    in_flight: Vec<InFlightTx<Input>>,
    completed: Vec<Input>,

    // Observability
    latency_tracker: ComponentStateHandle<L1SenderState>,
    backoff: ExponentialBackoff,

    // Reusable buffer for recv_many to avoid per-call allocation
    cmd_buffer: Vec<L1SenderCommand<Input>>,
}

impl<Input, F, P> L1SenderLoop<Input, F, P>
where
    Input: SendToL1,
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum> + Clone + 'static,
{
    // ==============================================================================
    // Main loop
    // ==============================================================================

    async fn run(mut self) -> anyhow::Result<()> {
        // Passthrough handling runs before the main loop and remains Fatal —
        // if the pipeline protocol is violated at startup, we can't continue.
        let command_name = Input::NAME;
        if process_prepending_passthrough_commands(
            &mut self.inbound,
            &self.outbound,
            &self.latency_tracker,
            command_name,
        )
        .await?
        .is_none()
        {
            tracing::info!(
                command_name,
                "inbound channel closed during passthrough phase"
            );
            return Ok(());
        }

        loop {
            // Phase 1: receive commands from upstream (only when the pipeline is empty).
            // All three collections being empty means a full cycle just completed.
            if self.pending_commands.is_empty()
                && self.in_flight.is_empty()
                && self.completed.is_empty()
            {
                match self.receive().await {
                    Ok(()) => {}
                    // receive() only returns Fatal (channel closed = upstream crashed)
                    Err(e) => return Err(e.into_anyhow()),
                }
            }

            // Phase 2: send pending commands to L1, one at a time.
            // Gas/blob fee caps are checked before any tx is submitted.
            if !self.pending_commands.is_empty() {
                match self.send_pending().await {
                    Ok(()) => {}
                    Err(L1SendError::Transient(e)) => {
                        tracing::warn!(
                            ?e,
                            command_name,
                            "transient error during send, entering backoff"
                        );
                        L1_SENDER_METRICS.transient_errors.inc();
                        self.latency_tracker
                            .enter_state(L1SenderState::TransientBackoff);
                        let delay = self.backoff.next();
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    Err(L1SendError::Recoverable { reason, source }) => {
                        let state = match reason {
                            RecoverableReason::GasBlocked => L1SenderState::GasBlocked,
                            RecoverableReason::BlobFeeBlocked => L1SenderState::BlobFeeBlocked,
                            _ => L1SenderState::TransientBackoff,
                        };
                        tracing::warn!(
                            ?source,
                            ?reason,
                            command_name,
                            "recoverable error, waiting 30s"
                        );
                        L1_SENDER_METRICS.recoverable_errors[&reason_label(reason)].inc();
                        self.latency_tracker.enter_state(state);
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        continue;
                    }
                    Err(L1SendError::Fatal(e)) => return Err(e),
                }
            }

            // Phase 3: wait for all in-flight txs to be included on L1.
            if !self.in_flight.is_empty() {
                match self.wait_for_inclusion().await {
                    Ok(()) => {}
                    Err(L1SendError::Transient(e)) => {
                        tracing::warn!(
                            ?e,
                            command_name,
                            "transient error waiting for inclusion, entering backoff"
                        );
                        L1_SENDER_METRICS.transient_errors.inc();
                        self.latency_tracker
                            .enter_state(L1SenderState::TransientBackoff);
                        let delay = self.backoff.next();
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    Err(L1SendError::Recoverable {
                        reason: RecoverableReason::TxTimeout,
                        source,
                    }) => {
                        // The timed-out tx was re-queued in pending_commands by wait_for_inclusion().
                        // Enter backoff and let the loop retry via send_pending().
                        tracing::warn!(
                            ?source,
                            command_name,
                            "tx timed out, re-queued for resubmission"
                        );
                        L1_SENDER_METRICS.recoverable_errors
                            [&reason_label(RecoverableReason::TxTimeout)]
                            .inc();
                        self.latency_tracker
                            .enter_state(L1SenderState::WaitingL1Inclusion);
                        let delay = self.backoff.next();
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    Err(L1SendError::Recoverable { reason, source }) => {
                        tracing::warn!(
                            ?source,
                            ?reason,
                            command_name,
                            "recoverable error waiting for inclusion"
                        );
                        L1_SENDER_METRICS.recoverable_errors[&reason_label(reason)].inc();
                        self.latency_tracker
                            .enter_state(L1SenderState::TransientBackoff);
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        continue;
                    }
                    Err(L1SendError::Fatal(e)) => return Err(e),
                }
            }

            // Phase 4: forward completed commands downstream.
            if !self.completed.is_empty() {
                match self.forward_downstream().await {
                    Ok(()) => {}
                    Err(e) => return Err(e.into_anyhow()),
                }
            }

            // A full cycle completed. Log balance/nonce (informational, non-fatal),
            // then reset the backoff counter.
            self.report_balance_and_nonce().await;
            self.backoff.reset();
        }
    }

    // ==============================================================================
    // Phase 1: receive
    // ==============================================================================

    /// Reads commands from the inbound channel into `pending_commands`.
    ///
    /// Blocks until at least one command arrives, then drains up to `command_limit`
    /// commands in one shot. Returns `Fatal` only if the channel is closed
    /// (upstream pipeline component crashed).
    async fn receive(&mut self) -> Result<(), L1SendError> {
        self.latency_tracker.enter_state(L1SenderState::WaitingRecv);
        let received = self
            .inbound
            .recv_many(&mut self.cmd_buffer, self.config.command_limit)
            .await;

        // recv_many returns 0 only when the channel is closed and empty.
        if received == 0 {
            tracing::info!(command_name = Input::NAME, "inbound channel closed");
            return Err(L1SendError::Fatal(anyhow::anyhow!(
                "inbound channel closed"
            )));
        }

        // Convert the raw channel commands into Input values.
        // Passthrough commands are not expected past the initial passthrough phase —
        // if one arrives here, that is a pipeline protocol violation (Fatal).
        for cmd in self.cmd_buffer.drain(..) {
            match cmd {
                L1SenderCommand::SendToL1(c) => self.pending_commands.push(c),
                L1SenderCommand::Passthrough(batch) => {
                    return Err(L1SendError::Fatal(anyhow::anyhow!(
                        "Unexpected passthrough command for batch {:?}. \
                         No passthrough commands are expected after the first `SendToL1`.",
                        batch.batch_number()
                    )));
                }
            }
        }

        let range = Input::display_range(&self.pending_commands);
        tracing::info!(
            command_name = Input::NAME,
            range,
            count = self.pending_commands.len(),
            "received commands from upstream"
        );
        L1_SENDER_METRICS.parallel_transactions[&Input::NAME]
            .set(self.pending_commands.len() as u64);
        Ok(())
    }

    // ==============================================================================
    // Phase 2: send_pending
    // ==============================================================================

    /// Sends each pending command to L1, moving them to `in_flight` on success.
    ///
    /// Gas and blob fee caps are checked against the current network estimates
    /// before any transaction is submitted. If fees exceed the cap, we return
    /// `Recoverable::GasBlocked` or `Recoverable::BlobFeeBlocked` and send nothing.
    ///
    /// Commands are sent one at a time so that partial progress is preserved: if
    /// the 3rd of 5 fails, the first 2 are already in `in_flight` and only the
    /// remaining 3 need to be retried.
    async fn send_pending(&mut self) -> Result<(), L1SendError> {
        self.latency_tracker.enter_state(L1SenderState::SendingToL1);

        // Estimate EIP-1559 fees once for the whole batch.
        // This is a Transient error path — the RPC may be temporarily unavailable.
        let eip1559_est = self
            .provider
            .estimate_eip1559_fees()
            .await
            .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?;

        L1_SENDER_METRICS.report_l1_eip_1559_estimation(eip1559_est);

        // If the network's estimated base fee exceeds our configured cap, there is no
        // point submitting a transaction — it would be stuck waiting for a lower base fee
        // anyway. Enter GasBlocked and wait for congestion to pass.
        if eip1559_est.max_fee_per_gas > self.config.max_fee_per_gas_wei {
            return Err(L1SendError::Recoverable {
                reason: RecoverableReason::GasBlocked,
                source: anyhow::anyhow!(
                    "network max_fee_per_gas {} exceeds configured cap {}",
                    eip1559_est.max_fee_per_gas,
                    self.config.max_fee_per_gas_wei
                ),
            });
        }

        // Use the minimum of the network estimate and our configured cap for both fee fields.
        let max_fee_per_gas = eip1559_est.max_fee_per_gas;
        let max_priority_fee_per_gas = eip1559_est
            .max_priority_fee_per_gas
            .min(self.config.max_priority_fee_per_gas_wei);

        while !self.pending_commands.is_empty() {
            // Peek at the first command without consuming it until we know the send succeeded.
            // We send commands in order so that L1 nonces are allocated in the correct sequence.
            let cmd = &self.pending_commands[0];

            // Build the base transaction request with gas parameters.
            let mut tx_request = TransactionRequest::default()
                .with_from(self.operator_address)
                .with_to(self.to_address)
                .with_input(cmd.solidity_call(self.gateway, &self.operator_address))
                .with_max_fee_per_gas(max_fee_per_gas)
                .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
                // Default value for `max_aggregated_tx_gas` from zksync-era
                .with_gas_limit(15_000_000);

            // Attach a blob sidecar if the command requires EIP-4844.
            if let Some(blob_sidecar) = cmd.blob_sidecar() {
                let fee_per_blob_gas = self
                    .provider
                    .get_blob_base_fee()
                    .await
                    .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?;

                L1_SENDER_METRICS.report_blob_base_fee(fee_per_blob_gas);

                // Refuse to submit a blob tx if the blob fee exceeds our cap.
                // Current behavior was to warn and send anyway — new behavior: block.
                if fee_per_blob_gas > self.config.max_fee_per_blob_gas_wei {
                    return Err(L1SendError::Recoverable {
                        reason: RecoverableReason::BlobFeeBlocked,
                        source: anyhow::anyhow!(
                            "blob base fee {} exceeds configured cap {}",
                            fee_per_blob_gas,
                            self.config.max_fee_per_blob_gas_wei
                        ),
                    });
                }

                tx_request.set_max_fee_per_blob_gas(self.config.max_fee_per_blob_gas_wei);
                tx_request.set_blob_sidecar(blob_sidecar);
            }

            // Fill the transaction (nonce, gas estimate) using the provider.
            // try_into_envelope / try_into_pooled are local conversion steps —
            // failure here indicates malformed tx data (Fatal), not an RPC issue.
            let envelope = self
                .provider
                .fill(tx_request)
                .await
                .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?
                .try_into_envelope()
                .map_err(|e| L1SendError::Fatal(anyhow::Error::from(e)))?
                .try_into_pooled()
                .map_err(|e| L1SendError::Fatal(anyhow::Error::from(e)))?;

            // Fetch the pending block to decide whether to use EIP-7594 blob format.
            // Falls back to the latest block if the pending block is unavailable (Infura quirk).
            let pending_block = self
                .provider
                .get_block(BlockId::pending())
                .await
                .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?;
            let block = match pending_block {
                Some(b) => b,
                None => self
                    .provider
                    .get_block(BlockId::latest())
                    .await
                    .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?
                    .ok_or_else(|| {
                        L1SendError::Transient(anyhow::anyhow!(
                            "no pending or latest block available"
                        ))
                    })?,
            };

            // todo: make conversion unconditional (and remove respective config) once anvil
            //       supports EIP-7594 blobs (see https://github.com/foundry-rs/foundry/issues/12222)
            let tx = if self.config.fusaka_upgrade_timestamp <= block.header.timestamp {
                envelope
                    .try_map_eip4844(|tx| {
                        tx.try_map_sidecar(|sidecar| {
                            Ok::<_, BlobTransactionValidationError>(
                                BlobTransactionSidecarVariant::Eip7594(sidecar.try_into_eip7594()?),
                            )
                        })
                    })
                    .map_err(|e| L1SendError::Fatal(anyhow::anyhow!("{e:?}")))?
            } else {
                envelope
            };

            // Submit the transaction. Parse the RPC error to distinguish nonce conflicts
            // from generic network failures.
            let pending_builder = self
                .provider
                .send_raw_transaction(&tx.encoded_2718())
                .await
                .map_err(|e| L1SendError::classify_send_raw_error(anyhow::Error::from(e)))?;

            let tx_hash = *pending_builder.tx_hash();
            let receipt_future = pending_builder
                .with_required_confirmations(1)
                .with_timeout(Some(TRANSACTION_TIMEOUT))
                .get_receipt()
                .boxed();

            // Consume the command from pending and move it to in_flight.
            let mut cmd = self.pending_commands.remove(0);
            cmd.as_mut()
                .iter_mut()
                .for_each(|envelope| envelope.set_stage(Input::SENT_STAGE));
            self.in_flight.push(InFlightTx {
                command: cmd,
                tx_hash,
                receipt_future,
            });
        }

        Ok(())
    }

    // ==============================================================================
    // Phase 3: wait_for_inclusion
    // ==============================================================================

    /// Awaits receipt futures for all in-flight txs in order.
    ///
    /// L1 transactions are ordered by sender nonce, so we wait for them in the
    /// order they were submitted. Successfully included txs move to `completed`.
    ///
    /// On timeout the command is moved back to `pending_commands` for resubmission.
    /// Known limitation: the timed-out tx may still be in the L1 mempool. If it
    /// lands before the retry, the retry will fail with `NonceTooLow` (recoverable).
    async fn wait_for_inclusion(&mut self) -> Result<(), L1SendError> {
        self.latency_tracker
            .enter_state(L1SenderState::WaitingL1Inclusion);

        while let Some(tx) = self.in_flight.first_mut() {
            match (&mut tx.receipt_future).await {
                Ok(receipt) => {
                    if receipt.status() {
                        // Tx succeeded — report metrics (non-fatal) and move to completed.
                        let cmd = self.in_flight.remove(0);
                        L1_SENDER_METRICS.report_tx_receipt(&cmd.command, receipt);
                        self.completed.push(cmd.command);
                    } else {
                        // Tx reverted on L1. The gas was already burned; manual intervention
                        // is required to diagnose the contract/calldata issue.
                        let cmd = self.in_flight.remove(0);
                        validate_tx_receipt_reverted(&self.provider, &cmd.command, receipt)
                            .await
                            .map_err(L1SendError::Fatal)?;
                        unreachable!("validate_tx_receipt_reverted always returns Err");
                    }
                }
                Err(PendingTransactionError::TxWatcher(WatchTxError::Timeout)) => {
                    // The BoxFuture is consumed after a Timeout error; we cannot re-poll it.
                    // Re-queue the command for resubmission via send_pending().
                    // Known limitation: the original tx may still be in the mempool and land later,
                    // causing a NonceTooLow on the retry (which is also recoverable).
                    let tx_hash = tx.tx_hash;
                    let timed_out = self.in_flight.remove(0);
                    tracing::warn!(
                        ?tx_hash,
                        command_name = Input::NAME,
                        "L1 transaction timed out waiting for inclusion; \
                         re-queuing command for resubmission. \
                         Known limitation: original tx may still be in mempool."
                    );
                    self.pending_commands.insert(0, timed_out.command);
                    return Err(L1SendError::Recoverable {
                        reason: RecoverableReason::TxTimeout,
                        source: anyhow::anyhow!("transaction {} timed out", tx_hash),
                    });
                }
                Err(e) => {
                    // Any other error (transport failure, dropped tx) is treated as transient.
                    return Err(L1SendError::Transient(anyhow::anyhow!("{e}")));
                }
            }
        }

        Ok(())
    }

    // ==============================================================================
    // Phase 4: forward_downstream
    // ==============================================================================

    /// Drains `completed` commands and sends them to the outbound channel.
    ///
    /// `outbound.send().await` only errors when the receiver is dropped, which
    /// means a downstream component crashed — Fatal.
    async fn forward_downstream(&mut self) -> Result<(), L1SendError> {
        self.latency_tracker.enter_state(L1SenderState::WaitingSend);

        for command in self.completed.drain(..) {
            for mut output_envelope in command.into() {
                output_envelope.set_stage(Input::MINED_STAGE);
                self.outbound
                    .send(output_envelope)
                    .await
                    .map_err(|e| L1SendError::Fatal(anyhow::Error::from(e)))?;
            }
        }

        Ok(())
    }

    // ==============================================================================
    // Helpers
    // ==============================================================================

    /// Reports operator balance and nonce after a successful send cycle.
    ///
    /// These calls are informational — RPC failures are logged at WARN level
    /// and do not affect the send loop.
    async fn report_balance_and_nonce(&mut self) {
        match self.provider.get_balance(self.operator_address).await {
            Ok(balance) => {
                let balance_str = format_ether(balance);
                if let Ok(v) = balance_str.parse::<f64>() {
                    L1_SENDER_METRICS.balance[&Input::NAME].set(v);
                } else {
                    tracing::warn!("failed to parse balance for metrics");
                }
                tracing::info!(
                    command_name = Input::NAME,
                    balance = balance_str,
                    "operator balance after send cycle"
                );
            }
            Err(e) => tracing::warn!(?e, "failed to fetch operator balance"),
        }

        match self
            .provider
            .get_transaction_count(self.operator_address)
            .await
        {
            Ok(nonce) => {
                L1_SENDER_METRICS.nonce[&Input::NAME].set(nonce);
            }
            Err(e) => tracing::warn!(?e, "failed to fetch operator nonce"),
        }
    }
}

/// Converts a `RecoverableReason` to the Prometheus label string used by
/// `L1_SENDER_METRICS.recoverable_errors`.
fn reason_label(reason: RecoverableReason) -> &'static str {
    match reason {
        RecoverableReason::GasBlocked => "gas_blocked",
        RecoverableReason::BlobFeeBlocked => "blob_fee_blocked",
        RecoverableReason::TxTimeout => "tx_timeout",
        RecoverableReason::NonceTooLow => "nonce_too_low",
    }
}

// ==============================================================================
// Public entry point
// ==============================================================================

/// Runs the L1 sender for one command type (commit, prove, or execute).
///
/// Registers the operator, processes any initial passthrough commands, then
/// enters the main phase-based loop via [`L1SenderLoop`].
///
/// Note: the same provider (sender address) must not be used outside this
/// function — sharing it would cause nonce conflicts.
pub async fn run_l1_sender<Input: SendToL1>(
    inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: Sender<SignedBatchEnvelope<FriProof>>,
    to_address: Address,
    mut provider: FillProvider<
        impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
        impl Provider<Ethereum> + Clone + 'static,
    >,
    config: L1SenderConfig<Input>,
    gateway: bool,
) -> anyhow::Result<()> {
    let latency_tracker =
        ComponentStateReporter::global().handle_for(Input::NAME, L1SenderState::WaitingRecv);

    let operator_address =
        register_operator::<_, Input>(&mut provider, config.operator_signer.clone()).await?;

    let cmd_buffer_capacity = config.command_limit;
    L1SenderLoop {
        inbound,
        outbound,
        provider,
        config,
        to_address,
        operator_address,
        gateway,
        pending_commands: Vec::new(),
        in_flight: Vec::new(),
        completed: Vec::new(),
        latency_tracker,
        backoff: ExponentialBackoff::new(Duration::from_secs(5), Duration::from_secs(60)),
        cmd_buffer: Vec::with_capacity(cmd_buffer_capacity),
    }
    .run()
    .await
}

// ==============================================================================
// Helper free functions
// ==============================================================================

async fn process_prepending_passthrough_commands<Input: SendToL1>(
    inbound: &mut PeekableReceiver<L1SenderCommand<Input>>,
    outbound: &Sender<SignedBatchEnvelope<FriProof>>,
    latency_tracker: &ComponentStateHandle<L1SenderState>,
    command_name: &str,
) -> anyhow::Result<Option<()>> {
    loop {
        latency_tracker.enter_state(L1SenderState::WaitingRecv);
        match inbound
            .peek_recv(|command| matches!(command, L1SenderCommand::Passthrough(_)))
            .await
        {
            None => return Ok(None),
            // command is SendToL1 (not passthrough)
            // we don't expect anymore passthroughs and can proceed with normal operations
            Some(false) => return Ok(Some(())),
            // command is passthrough
            Some(true) => {
                let Some(next_command) = inbound.recv().await else {
                    return Ok(None);
                };
                match next_command {
                    L1SenderCommand::SendToL1(_) => {
                        anyhow::bail!("Mismatch between peeked and received command")
                    }
                    L1SenderCommand::Passthrough(batch) => {
                        tracing::info!(
                            command_name,
                            batch_number = batch.batch_number(),
                            "Not actually sending to L1, just passing through"
                        );
                        latency_tracker.enter_state(L1SenderState::WaitingSend);
                        outbound
                            .send((*batch).with_stage(Input::PASSTHROUGH_STAGE))
                            .await?;
                    }
                }
            }
        }
    }
}

async fn register_operator<
    P: Provider + WalletProvider<Wallet = EthereumWallet>,
    Input: SendToL1,
>(
    provider: &mut P,
    signer_config: SignerConfig,
) -> anyhow::Result<Address> {
    let address = signer_config
        .register_with_wallet(provider.wallet_mut())
        .await?;

    let balance = provider.get_balance(address).await?;
    if let Ok(v) = format_ether(balance).parse::<f64>() {
        L1_SENDER_METRICS.balance[&Input::NAME].set(v);
    } else {
        tracing::warn!("failed to parse operator balance for metrics");
    }
    let address_string: &'static str = address.to_string().leak();
    L1_SENDER_METRICS.l1_operator_address[&(Input::NAME, address_string)].set(1);

    if balance.is_zero() {
        anyhow::bail!("L1 sender's address {address} has zero balance");
    }

    tracing::info!(
        command_name = Input::NAME,
        balance_eth = format_ether(balance),
        %address,
        "initialized L1 sender",
    );
    Ok(address)
}

/// Logs full diagnostic info for a reverted L1 transaction and returns `Err`.
///
/// A revert is always fatal: gas was already burned and manual inspection of
/// the contract/calldata is required.
async fn validate_tx_receipt_reverted<Input: SendToL1>(
    provider: &impl Provider,
    command: &Input,
    receipt: TransactionReceipt,
) -> anyhow::Result<()> {
    tracing::error!(
        %command,
        tx_hash = ?receipt.transaction_hash,
        l1_block_number = receipt.block_number.unwrap(),
        "Transaction failed on L1",
    );
    if let Ok(trace) = provider
        .debug_trace_transaction(
            receipt.transaction_hash,
            GethDebugTracingOptions::call_tracer(CallConfig::default()),
        )
        .await
    {
        let call_frame = trace
            .try_into_call_frame()
            .expect("requested call tracer but received a different call frame type");
        tracing::error!(
            ?call_frame.output,
            ?call_frame.error,
            ?call_frame.revert_reason,
            "Failed transaction's top-level call frame"
        );
    }
    anyhow::bail!(
        "{} L1 command transaction failed, see L1 transaction's trace for more details (tx_hash='{:?}')",
        command,
        receipt.transaction_hash
    );
}
