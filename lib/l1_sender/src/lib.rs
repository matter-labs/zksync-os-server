pub mod commands;
pub mod config;
mod metrics;
pub mod pipeline_component;
mod pipelined;
pub mod upgrade_gatekeeper;

use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderFeeConfig;
use crate::metrics::{L1_SENDER_METRICS, PriorityFeeEstimatePercentile, PriorityFeeEstimateWindow};
use crate::pipeline_component::L1Sender;
use alloy::consensus::Transaction as ConsensusTransaction;
use alloy::eips::eip4844::{DATA_GAS_PER_BLOB, env_settings::EnvKzgSettings};
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::{TransactionBuilder, TransactionBuilder4844, TransactionResponse};
use alloy::primitives::utils::{format_ether, format_units};
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::Provider;
use alloy::providers::ext::DebugApi;
use alloy::providers::utils::Eip1559Estimation;
use alloy::rpc::types::simulate::{SimBlock, SimulatePayload};
use alloy::rpc::types::state::{AccountOverride, StateOverridesBuilder};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::transports::TransportError;
use anyhow::Context as _;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState, StateLabel};
use zksync_os_pipeline::{PeekableReceiver, SendAndRecordExt};
use zksync_os_provider::EthWalletProvider;

/// Component-specific state for the L1 sender.
pub enum L1SenderState {
    /// Waiting for the next batch to commit/prove/execute.
    Idle,
    /// Submitting a transaction to L1.
    SendingToL1,
    /// Transaction submitted; waiting for L1 block inclusion.
    WaitingL1Inclusion,
}

impl StateLabel for L1SenderState {
    fn generic(&self) -> GenericComponentState {
        match self {
            Self::Idle => GenericComponentState::Idle,
            Self::SendingToL1 => GenericComponentState::Active,
            Self::WaitingL1Inclusion => GenericComponentState::Active,
        }
    }
    fn specific(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::SendingToL1 => "sending_to_l1",
            Self::WaitingL1Inclusion => "waiting_l1_inclusion",
        }
    }
}

/// A code for "method not found" error response as declared in JSON-RPC 2.0 spec.
const METHOD_NOT_FOUND_CODE: i64 = -32601;
/// Future that resolves into a (fallible) transaction receipt.
type TransactionReceiptFuture = BoxFuture<'static, anyhow::Result<TransactionReceipt>>;
type PendingTx<Input> = (TransactionReceiptFuture, Input, Instant);

const OPERATOR_METRICS_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Per-tx gas limit used when `eth_simulateV1` cannot produce a usable estimate.
/// Sized to cover the bounded set of commit/prove/execute calls.
const L1_GAS_LIMIT_FALLBACK: u64 = 15_000_000;
/// Per-call cap for `eth_simulateV1`. The simulation reports the actual `gas_used`.
const L1_SIM_GAS_LIMIT: u64 = 30_000_000;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FeeParams {
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    max_fee_per_blob_gas: u128,
}

impl FeeParams {
    /// Per-field maximum of two fee sets. Used to apply a replacement-fee floor on top of
    /// the regularly resolved fees.
    pub(crate) fn max(self, other: FeeParams) -> FeeParams {
        FeeParams {
            max_fee_per_gas: self.max_fee_per_gas.max(other.max_fee_per_gas),
            max_priority_fee_per_gas: self
                .max_priority_fee_per_gas
                .max(other.max_priority_fee_per_gas),
            max_fee_per_blob_gas: self.max_fee_per_blob_gas.max(other.max_fee_per_blob_gas),
        }
    }
}

/// A blob sidecar converted once into the wire format the chain accepts.
///
/// The EIP-7594 conversion computes 128 KZG cell proofs per blob (~200ms of CPU each), so it
/// must happen exactly once per command — the result is shared between gas simulation, the
/// send path, and in-flight simulation prefixes.
#[derive(Debug, Clone)]
pub(crate) struct PreparedSidecar {
    variant: BlobTransactionSidecarVariant,
    blob_count: u64,
}

impl PreparedSidecar {
    /// Converts `sidecar` into the format the chain's active fork accepts. The EIP-7594 cell
    /// proof computation is CPU-heavy, so it runs on the blocking pool.
    pub(crate) async fn prepare(
        sidecar: alloy::consensus::BlobTransactionSidecar,
        use_eip7594: bool,
    ) -> anyhow::Result<Self> {
        let blob_count = sidecar.blobs.len() as u64;
        let variant = if use_eip7594 {
            let converted = tokio::task::spawn_blocking(move || {
                sidecar.try_into_7594(EnvKzgSettings::Default.get())
            })
            .await
            .context("EIP-7594 sidecar conversion task panicked")??;
            BlobTransactionSidecarVariant::Eip7594(converted)
        } else {
            BlobTransactionSidecarVariant::Eip4844(sidecar)
        };
        Ok(Self {
            variant,
            blob_count,
        })
    }

    pub(crate) fn blob_count(&self) -> u64 {
        self.blob_count
    }
}

/// An in-flight (submitted, not yet observed mined) transaction mirrored into `eth_simulateV1`
/// payloads so that gas estimates for chained commands (batch N+1 requires batch N committed)
/// stay exact while predecessors are still in the mempool. On providers whose simulation base
/// already includes pool transactions the prefix call simply reverts without applying state,
/// which leaves the chained state intact either way; prefix results are always discarded.
pub(crate) struct SimPrefixEntry {
    pub(crate) nonce: u64,
    pub(crate) calldata: Bytes,
    pub(crate) sidecar: Option<Arc<PreparedSidecar>>,
    pub(crate) fee_params: FeeParams,
}

fn build_l1_simulation_request(
    operator_address: Address,
    to_address: Address,
    input: Bytes,
    nonce: u64,
    fee_params: FeeParams,
    prepared_sidecar: Option<&PreparedSidecar>,
) -> TransactionRequest {
    // Mirror submission fees so providers (e.g. anvil) that parse the request
    // as a typed tx accept it.
    let mut req = TransactionRequest::default()
        .with_from(operator_address)
        .with_to(to_address)
        .with_input(input)
        .with_max_fee_per_gas(fee_params.max_fee_per_gas)
        .with_max_priority_fee_per_gas(fee_params.max_priority_fee_per_gas)
        .with_nonce(nonce)
        .with_gas_limit(L1_SIM_GAS_LIMIT);

    if let Some(prepared_sidecar) = prepared_sidecar {
        req.max_fee_per_blob_gas = Some(fee_params.max_fee_per_blob_gas);
        req.set_blob_sidecar(prepared_sidecar.variant.clone());
        // Anvil routes blob requests through the EIP-4844 arm only when
        // `type=3` is set explicitly; otherwise it returns -32602.
        req.transaction_type = Some(3);
    }

    req
}

/// Process responsible for sending transactions to L1.
/// Handles one type of l1 command (e.g. Commit or Prove).
/// Keeps up to `command_limit` transactions in flight, forwarding each command to the output
/// channel once its transaction is confirmed — see `pipelined` for the send machinery.
///
/// Important: the same provider (sender address) must not be used outside this process.
///     Otherwise, there will be a nonce conflict and a failed L1 transaction
///     (recoverable on restart)
///
/// Known issues:
///   * Crashes when there is a gap in incoming L1 blocks (happens periodically with Infura provider)
///
/// Note: we pass `to_address` - L1 contract address to send transactions to.
/// It differs between commit/prove/execute (e.g., timelock vs diamond proxy)
impl<Input> L1Sender<Input>
where
    Input: SendToL1 + Send + 'static,
{
    pub async fn operator_address(&self) -> anyhow::Result<Address> {
        self.config.operator_signer.address().await
    }

    pub async fn run_l1_sender(
        &self,
        // == plumbing ==
        mut inbound: PeekableReceiver<L1SenderCommand<Input>>,
        outbound: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        // Process all potential passthrough commands first
        if self
            .process_prepending_passthrough_commands(&mut inbound, &outbound, &state_reporter)
            .await?
            .is_none()
        {
            tracing::info!("inbound channel closed");
            return Ok(());
        }

        // The KZG trusted setup loads lazily on first use (~seconds); warm it up off the hot
        // path so the first blob-carrying send doesn't stall the async executor.
        if self.provider.capabilities().supports_eip7594 {
            tokio::task::spawn_blocking(|| {
                let _ = EnvKzgSettings::Default.get();
            })
            .await
            .context("KZG trusted setup preload task panicked")?;
        }

        // Both send paths are large state machines; boxing keeps them out of this future
        // (and of the component future moved by value at pipeline spawn), whose size is
        // stack-critical — see the stack-overflow note in `pipelined.rs`.
        if self.config.pipelining_enabled {
            Box::pin(self.run_pipelined(inbound, outbound, state_reporter)).await
        } else {
            Box::pin(self.run_stop_and_wait(inbound, outbound, state_reporter)).await
        }
    }

    /// Fallback send path (`pipelining_enabled = false`): drain up to `command_limit`
    /// commands, send them, wait for all receipts + confirmations, repeat.
    async fn run_stop_and_wait(
        &self,
        mut inbound: PeekableReceiver<L1SenderCommand<Input>>,
        outbound: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
        state_reporter: ComponentStateReporter,
    ) -> anyhow::Result<()> {
        let command_name = Input::COMPONENT_ID.as_str();
        let fee_config = self.config.fee_config;
        let force_transaction_resubmission = self.config.force_transaction_resubmission;

        let mut cmd_buffer = Vec::with_capacity(self.config.command_limit);

        // On startup, either recover submitted transactions from a previous session or, when
        // explicitly requested, skip recovery so the normal send path replaces them.
        let recovered = if force_transaction_resubmission {
            vec![]
        } else {
            match self
                .recover_in_flight_txs(&mut inbound, &state_reporter)
                .await
            {
                Ok(paired) => paired,
                Err(err) => {
                    tracing::warn!("Error during in-flight transaction recovery: {err}");
                    vec![]
                }
            }
        };

        // Wait for any recovered in-flight transactions to be mined before accepting
        // new commands. Their nonces precede anything we are about to send, so they
        // must be confirmed first.
        if !recovered.is_empty() {
            let pending_txs: Vec<PendingTx<Input>> = recovered
                .into_iter()
                .map(|(tx_hash, cmd)| {
                    let fut = self.wait_for_confirmed_receipt(tx_hash);
                    (fut, cmd, Instant::now())
                })
                .collect();
            self.wait_for_txs_and_forward(pending_txs, &state_reporter, &outbound)
                .await?;
        }

        // At this point, recovered in-flight transactions are confirmed. If force resubmission is
        // enabled, the queued commands stay in `inbound` and the normal send path replaces them.
        // Only actual SendToL1 commands are expected from here on.
        loop {
            state_reporter.enter_state(L1SenderState::Idle);
            // Sleeps until at least one command is available, then greedily drains up to
            // command_limit items without waiting. cmd_buffer is emptied every iteration.
            let received = inbound
                .recv_many(&mut cmd_buffer, self.config.command_limit)
                .await;
            // Only returns 0 when the channel is closed and drained.
            if received == 0 {
                tracing::info!("inbound channel closed");
                return Ok(());
            }
            let last = cmd_buffer
                .last()
                .context("recv_many returned non-zero count but cmd_buffer is empty")?;
            state_reporter.record_picked(
                last.last_block_number(),
                last.block_timestamp(),
                Some(last.last_batch_number()),
            );
            let commands = cmd_buffer
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
            let range = Input::display_range(&commands); // Only for logging
            tracing::info!(command_name, range, "sending L1 transactions");
            L1_SENDER_METRICS.parallel_transactions[&command_name].set(commands.len() as u64);

            let operator_address = self.operator_address().await?;
            // One pending-count read per cycle. The account is quiescent here (prior cycle's txs
            // are confirmed), so this baseline is race-free; used for both simulation and sends.
            let base_nonce = self
                .provider
                .get_transaction_count(operator_address)
                .pending()
                .await
                .context("get pending nonce for L1 sender cycle")?;
            // The only fee read per send cycle (one drain of up to `command_limit`
            // commands); the send loop below reuses these params for every command instead
            // of resolving again per tx. That's safe because fee caps are config-bound and
            // whether a fee is high enough is decided when the tx is mined, not now — so
            // per-command reads would only add an RPC round-trip per tx without changing
            // what we submit.
            let fee_params = self
                .resolve_fee_params(fee_config, force_transaction_resubmission)
                .await?;
            let prepared_sidecars = self.prepare_sidecars(&commands).await?;
            let gas_limits = self
                .estimate_gas_limits(
                    &[],
                    &commands,
                    &prepared_sidecars,
                    operator_address,
                    fee_params,
                    base_nonce,
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

            // It's important to preserve the order of commands -
            // so that we send them downstream also in order.
            // This holds true because l1 transactions are included in the order of sender nonce.
            // Keep this in mind if changing sending logic (that is, if adding `buffer` we'd need to set nonce manually)
            let pending_txs: Vec<PendingTx<Input>> = futures::stream::iter(
                commands
                    .into_iter()
                    .zip(gas_limits)
                    .zip(prepared_sidecars)
                    .enumerate(),
            )
            .then(|(nonce_offset, ((mut cmd, gas_limit), prepared_sidecar))| {
                let range = range.clone();
                async move {
                    let tx_request = self.build_tx_request(
                        cmd.solidity_call(&operator_address),
                        operator_address,
                        base_nonce + nonce_offset as u64,
                        gas_limit,
                        fee_params,
                        prepared_sidecar.as_deref(),
                        blob_base_fee,
                    );

                    // Notify CommitWatcher before the transaction can possibly land on L1:
                    // this batch number is being submitted by this session.
                    self.note_submitted_batches(&cmd);

                    let pending_tx = self.send_tx_with_retries(tx_request, &range).await?;
                    let submitted_at = Instant::now();
                    let tx_hash = *pending_tx.tx_hash();
                    let receipt_fut = self.wait_for_confirmed_receipt(tx_hash);
                    tracing::info!(
                        "{command_name}: L1 transaction submitted for {range}. Hash: {tx_hash:?} Waiting for inclusion...",
                    );

                    cmd.as_mut()
                        .iter_mut()
                        .for_each(|envelope| envelope.set_stage(Input::SENT_STAGE));
                    anyhow::Ok((receipt_fut, cmd, submitted_at))
                }
            })
            // Transactions are sent sequentially and only waited on in parallel.
            .try_collect::<Vec<_>>()
            .await?;
            tracing::info!(command_name, range, "sent to L1, waiting for inclusion");
            self.wait_for_txs_and_forward(pending_txs, &state_reporter, &outbound)
                .await?;
        }
    }

    /// Converts every blob-carrying command's sidecar into the chain's accepted wire format
    /// exactly once. The result is shared between gas simulation, the send path and
    /// in-flight simulation prefixes; the EIP-7594 conversion computes KZG cell proofs and
    /// must never run twice for the same sidecar.
    pub(crate) async fn prepare_sidecars(
        &self,
        commands: &[Input],
    ) -> anyhow::Result<Vec<Option<Arc<PreparedSidecar>>>> {
        let use_eip7594 = self.provider.capabilities().supports_eip7594;
        futures::future::try_join_all(commands.iter().map(|cmd| async move {
            match cmd.blob_sidecar() {
                Some(sidecar) => Ok(Some(Arc::new(
                    PreparedSidecar::prepare(sidecar, use_eip7594).await?,
                ))),
                None => Ok(None),
            }
        }))
        .await
    }

    /// Only blob-carrying commands (commit path) need the blob base fee, so fetch it once per
    /// send wave instead of paying an RPC round-trip per command. Used for monitoring only —
    /// the submitted cap always comes from the configured fee params.
    pub(crate) async fn fetch_blob_base_fee_if_needed(
        &self,
        prepared_sidecars: &[Option<Arc<PreparedSidecar>>],
    ) -> anyhow::Result<Option<u128>> {
        if prepared_sidecars.iter().any(|sidecar| sidecar.is_some()) {
            let fee = self.provider.get_blob_base_fee().await?;
            L1_SENDER_METRICS.report_blob_base_fee(fee)?;
            Ok(Some(fee))
        } else {
            Ok(None)
        }
    }

    /// Builds a submission-ready transaction request and reports the balance-required metric.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_tx_request(
        &self,
        calldata: Bytes,
        operator_address: Address,
        nonce: u64,
        gas_limit: u64,
        fee_params: FeeParams,
        prepared_sidecar: Option<&PreparedSidecar>,
        blob_base_fee: Option<u128>,
    ) -> TransactionRequest {
        let mut tx_request = TransactionRequest::default()
            .with_from(operator_address)
            .with_nonce(nonce)
            .with_max_fee_per_gas(fee_params.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fee_params.max_priority_fee_per_gas)
            .with_gas_limit(gas_limit)
            .with_to(self.to_address)
            .with_input(calldata);

        let mut blob_gas_limit = 0;
        if let Some(prepared_sidecar) = prepared_sidecar {
            blob_gas_limit = prepared_sidecar.blob_count() * DATA_GAS_PER_BLOB;
            let max_fee_per_blob_gas = fee_params.max_fee_per_blob_gas;
            if let Some(fee_per_blob_gas) = blob_base_fee
                && fee_per_blob_gas > max_fee_per_blob_gas
            {
                tracing::warn!(
                    max_fee_per_blob_gas,
                    fee_per_blob_gas,
                    "L1 sender's configured maxFeePerBlobGas is lower than the one estimated from network"
                );
            }
            tx_request.set_max_fee_per_blob_gas(max_fee_per_blob_gas);
            // The sidecar already carries the EIP-7594 or EIP-4844 format matching the chain's
            // active fork (probed via `eth_config`, falling back to a chain-id heuristic — see
            // `ProviderCapabilities::supports_eip7594` and
            // https://github.com/foundry-rs/foundry/issues/12222).
            tx_request.set_blob_sidecar(prepared_sidecar.variant.clone());
        }

        let execution_balance_required =
            tx_request.max_fee_per_gas.unwrap_or_default() * u128::from(gas_limit);
        let blob_balance_required =
            tx_request.max_fee_per_blob_gas.unwrap_or_default() * u128::from(blob_gas_limit);
        let balance_required = execution_balance_required
            .saturating_add(blob_balance_required)
            .min(u128::from(u64::MAX)) as u64;
        L1_SENDER_METRICS.balance_required_for_tx[&Input::COMPONENT_ID.as_str()]
            .set(balance_required);

        tx_request
    }

    /// Notifies the CommitWatcher which batch numbers this session has submitted (or is about
    /// to submit) to L1, so a commit event for them is not mistaken for a leftover transaction
    /// from a crashed session.
    pub(crate) fn note_submitted_batches(&self, cmd: &Input) {
        if let Some(sender) = &self.commit_submitted_tx {
            let batch_number = cmd
                .as_ref()
                .last()
                .expect("every command contains at least one envelope")
                .batch_number();
            sender.send_if_modified(|current| {
                if batch_number > *current {
                    *current = batch_number;
                    true
                } else {
                    false
                }
            });
        }
    }

    /// Submits a transaction, retrying rejections that are known to be transient:
    ///
    /// * nonce-class rejections — a definitive refusal (tx not admitted), typically a transient
    ///   pool/state view inconsistency around a block import. The nonce is fixed, so re-sending
    ///   unchanged after a backoff self-heals.
    /// * pool-capacity rejections — the per-account or global pool limit is hit, e.g. when an
    ///   L1 reorg briefly returns already-mined transactions to the pool while our in-flight
    ///   window is full. Clears as blocks mine, so a longer retry budget applies.
    /// * fee-too-low rejections — the L1 fee market moved above our configured caps. The caps
    ///   are a deliberate operator bound, so the sender waits indefinitely (warning on the
    ///   transaction-timeout cadence) instead of crash-looping for the duration of a fee spike;
    ///   upstream backpressure engages while it waits.
    ///
    /// Everything else (including transport errors, where the tx may or may not have been
    /// admitted) propagates as fatal: restart + in-flight recovery is the only safe way to
    /// resolve the ambiguity without risking local nonce divergence.
    pub(crate) async fn send_tx_with_retries(
        &self,
        mut tx_request: TransactionRequest,
        range: &str,
    ) -> anyhow::Result<alloy::providers::PendingTransactionBuilder<alloy::network::Ethereum>> {
        let command_name = Input::COMPONENT_ID.as_str();
        let started_at = Instant::now();
        let mut nonce_error_attempt = 1;
        let mut pool_capacity_attempt = 1;
        let mut next_fee_warning_at = self.config.transaction_timeout;
        loop {
            match self.provider.send_transaction(tx_request.clone()).await {
                Ok(pending_tx) => return Ok(pending_tx),
                Err(err)
                    if is_nonce_error(&err)
                        && nonce_error_attempt < self.config.nonce_error_max_attempts =>
                {
                    tracing::warn!(
                        command_name,
                        range,
                        nonce_error_attempt,
                        %err,
                        "L1 node rejected the transaction with a nonce error; retrying"
                    );
                    tokio::time::sleep(self.config.nonce_error_retry_backoff).await;
                    nonce_error_attempt += 1;
                }
                Err(err)
                    if is_pool_capacity_error(&err)
                        && pool_capacity_attempt < POOL_CAPACITY_ERROR_MAX_ATTEMPTS =>
                {
                    tracing::warn!(
                        command_name,
                        range,
                        pool_capacity_attempt,
                        %err,
                        "L1 node rejected the transaction because the tx pool is at capacity; \
                         waiting for L1 to mine before retrying"
                    );
                    tokio::time::sleep(self.config.nonce_error_retry_backoff).await;
                    pool_capacity_attempt += 1;
                }
                Err(err) if is_fee_too_low_error(&err) => {
                    let elapsed = started_at.elapsed();
                    if !self.config.transaction_timeout.is_zero() && elapsed >= next_fee_warning_at
                    {
                        next_fee_warning_at += self.config.transaction_timeout;
                        tracing::warn!(
                            command_name,
                            range,
                            waited_secs = elapsed.as_secs_f64(),
                            %err,
                            "L1 fee market is above the configured fee caps; the sender is \
                             stalled until fees drop back under the caps (or the caps are \
                             raised in `l1_sender` config)"
                        );
                    } else {
                        tracing::info!(
                            command_name,
                            range,
                            %err,
                            "transaction priced below the current L1 fee market; retrying"
                        );
                    }
                    tokio::time::sleep(self.config.nonce_error_retry_backoff).await;
                    // Re-resolve fees before retrying: the market may have moved permanently
                    // past this wave's estimate while still being below the configured caps —
                    // retrying the stale estimate would stall until the market came back down.
                    match self
                        .resolve_fee_params(
                            self.config.fee_config,
                            self.config.force_transaction_resubmission,
                        )
                        .await
                    {
                        Ok(fresh) => {
                            // Fees only ratchet up: EIP-1559 replacement rules reject
                            // lowering, and the original fees may carry a replacement floor.
                            let raised = FeeParams {
                                max_fee_per_gas: tx_request
                                    .max_fee_per_gas
                                    .unwrap_or_default()
                                    .max(fresh.max_fee_per_gas),
                                max_priority_fee_per_gas: tx_request
                                    .max_priority_fee_per_gas
                                    .unwrap_or_default()
                                    .max(fresh.max_priority_fee_per_gas),
                                max_fee_per_blob_gas: tx_request
                                    .max_fee_per_blob_gas
                                    .unwrap_or_default()
                                    .max(fresh.max_fee_per_blob_gas),
                            };
                            tx_request.max_fee_per_gas = Some(raised.max_fee_per_gas);
                            tx_request.max_priority_fee_per_gas =
                                Some(raised.max_priority_fee_per_gas);
                            if tx_request.max_fee_per_blob_gas.is_some() {
                                tx_request.max_fee_per_blob_gas = Some(raised.max_fee_per_blob_gas);
                            }
                        }
                        Err(err) => {
                            // Estimation hiccups must not kill the wait loop; the next
                            // iteration retries with the current fees.
                            tracing::warn!(
                                command_name,
                                range,
                                %err,
                                "failed to refresh fee estimate while waiting out the fee \
                                 market; retrying with current fees"
                            );
                        }
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Waits for all pending L1 transaction receipts, validates them, logs balance/nonce
    /// metrics, and forwards the completed commands downstream.
    async fn wait_for_txs_and_forward(
        &self,
        pending_txs: Vec<PendingTx<Input>>,
        state_reporter: &ComponentStateReporter,
        outbound: &mpsc::Sender<SignedBatchEnvelope<FriProof>>,
    ) -> anyhow::Result<()> {
        let command_name = Input::COMPONENT_ID.as_str();
        state_reporter.enter_state(L1SenderState::WaitingL1Inclusion);

        let completed_commands: Vec<Input> = async {
            let mut completed = Vec::with_capacity(pending_txs.len());
            for (receipt_fut, command, submitted_at) in pending_txs.into_iter() {
                let receipt = receipt_fut.await;
                // Observe latency before propagating errors so timeout cases are recorded.
                L1_SENDER_METRICS.tx_inclusion_latency_seconds[&command_name]
                    .observe(submitted_at.elapsed().as_secs_f64());
                let receipt = receipt?;
                self.validate_tx_receipt(&command, receipt).await?;
                completed.push(command);
            }
            anyhow::Ok(completed)
        }
        .await?;

        let range = Input::display_range(&completed_commands);
        let operator_address = self.operator_address().await?;
        let balance = format_ether(self.provider.get_balance(operator_address).await?);
        let nonce = self
            .provider
            .get_transaction_count(operator_address)
            .await?;
        tracing::info!(
            command_name,
            range,
            balance,
            nonce,
            "all transactions included, sending downstream",
        );
        L1_SENDER_METRICS.balance[&command_name].set(balance.parse()?);
        L1_SENDER_METRICS.nonce[&command_name].set(nonce);

        for command in completed_commands {
            for mut output_envelope in command.into() {
                output_envelope.set_stage(Input::MINED_STAGE);
                outbound.send_and_record(output_envelope, state_reporter)?;
            }
        }
        Ok(())
    }

    pub(crate) fn wait_for_confirmed_receipt(&self, tx_hash: B256) -> TransactionReceiptFuture {
        let provider = self.provider.clone();
        let required_confirmations = self.config.required_confirmations;
        let timeout = self.config.transaction_timeout;
        let poll_interval = self.config.poll_interval;
        async move {
            let started_at = Instant::now();
            let mut next_warning_at = if timeout.is_zero() {
                None
            } else {
                Some(timeout)
            };

            loop {
                let latest_block = provider.get_block_number().await.map_err(|err| {
                tracing::warn!(
                    "Failed to fetch latest L1 block while waiting for transaction confirmation \
                 for tx {tx_hash}: {err}",
                );
                anyhow::Error::from(err)
            })?;
                let receipt = match provider.get_transaction_receipt(tx_hash).await {
                    Ok(receipt) => receipt,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to fetch transaction receipt while waiting for confirmation \
                     for tx {tx_hash}: {err}",
                        );
                        return Err(err.into());
                    }
                };
                if let Some(receipt) = receipt.as_ref() {
                    let receipt_block_number = receipt
                        .block_number
                        .context("transaction receipt missing block number")?;
                    let confirmed_at = receipt_block_number
                        .saturating_add(required_confirmations.saturating_sub(1));
                    if latest_block >= confirmed_at {
                        return Ok(receipt.clone());
                    }
                }

                let elapsed = started_at.elapsed();
                if let Some(warning_at) = next_warning_at
                    && elapsed >= warning_at
                {
                    let receipt_block_number =
                        receipt.as_ref().and_then(|receipt| receipt.block_number);
                    let confirmed_at = receipt_block_number
                        .map(|block| block + required_confirmations.saturating_sub(1));
                    tracing::warn!(
                        "Still waiting for L1 transaction confirmation for tx {tx_hash}. \
                 required_confirmations={required_confirmations}, \
                 waited_secs={}, latest_l1_block={latest_block}, \
                 receipt_block_number={receipt_block_number:?}, confirmed_at={confirmed_at:?}",
                        elapsed.as_secs_f64(),
                    );
                    next_warning_at = Some(warning_at + timeout);
                }

                tokio::time::sleep(poll_interval).await;
            }
        }
        .boxed()
    }

    /// Detects in-flight L1 transactions from a previous session, pairs each one with the
    /// corresponding queued command, and returns them ready to hand to the main loop.
    ///
    /// For each in-flight tx, the next command is peeked and its calldata is compared against
    /// the on-chain input. On a match the command is consumed and paired. On the first mismatch
    /// the loop stops and whatever has been paired so far is returned — the unmatched command
    /// remains in `inbound` for the normal send path.
    ///
    /// `l1_block_number` must be the same L1 block at which `getTotalBatches*` was called when
    /// constructing the inbound command queue. Pinning the confirmed-nonce baseline to that block
    /// prevents the race where txs mined between the `getTotalBatches` call and this nonce check
    /// cause us to mis-count in-flight txs and crash on calldata mismatch.
    async fn recover_in_flight_txs(
        &self,
        inbound: &mut PeekableReceiver<L1SenderCommand<Input>>,
        state_reporter: &ComponentStateReporter,
    ) -> anyhow::Result<Vec<(alloy::primitives::B256, Input)>> {
        let command_name = Input::COMPONENT_ID.as_str();
        let operator_address = self.operator_address().await?;
        let latest_nonce = self
            .provider
            .get_transaction_count(operator_address)
            .block_id(BlockId::number(self.l1_block_number))
            .await
            .context("get confirmed transaction count")?;
        let pending_nonce = self
            .provider
            .get_transaction_count(operator_address)
            .pending()
            .await
            .context("get pending transaction count")?;

        if pending_nonce <= latest_nonce {
            return Ok(vec![]);
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
                    "eth_getTransactionBySenderAndNonce is not supported by current provider.",
                );
                return Ok(vec![]);
            }
            anyhow::bail!("Error while probing eth_getTransactionBySenderAndNonce support: {e}");
        }

        // For each pending nonce, fetch the in-flight tx then peek at the next queued command.
        // If the command's calldata matches what is on-chain, consume and pair it. On the first
        // mismatch, stop — the unmatched command stays in `inbound` and will be re-sent by the
        // normal send path (replacing the in-flight tx at that nonce).
        let mut paired = Vec::with_capacity(in_flight_count);
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
                        "In-flight transaction at nonce {nonce} was dropped from the mempool.",
                    );
                    return Ok(paired);
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
                None => anyhow::bail!("inbound channel closed during in-flight recovery"),
                Some(false) => {
                    tracing::warn!(
                        command_name,
                        nonce,
                        "In-flight transaction calldata does not match the next queued command. \
                     Stopping recovery at nonce {nonce}.",
                    );
                    break;
                }
                Some(true) => {
                    let Some(L1SenderCommand::SendToL1(cmd)) =
                        inbound.recv_and_record_picked(state_reporter).await
                    else {
                        unreachable!("peek succeeded, recv must return the same item");
                    };
                    paired.push((tx.tx_hash(), cmd));
                }
            }
        }

        tracing::info!(
            command_name,
            recovered = paired.len(),
            in_flight_count,
            "Recovered in-flight transactions; will wait for their inclusion before accepting new commands",
        );

        Ok(paired)
    }

    async fn process_prepending_passthrough_commands(
        &self,
        inbound: &mut PeekableReceiver<L1SenderCommand<Input>>,
        outbound: &mpsc::Sender<SignedBatchEnvelope<FriProof>>,
        state_reporter: &ComponentStateReporter,
    ) -> anyhow::Result<Option<()>> {
        let command_name = Input::COMPONENT_ID.as_str();
        loop {
            state_reporter.enter_state(L1SenderState::Idle);
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
                    let Some(next_command) = inbound.recv_and_record_picked(state_reporter).await
                    else {
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
                            outbound.send_and_record(
                                (*batch).with_stage(Input::PASSTHROUGH_STAGE),
                                state_reporter,
                            )?;
                        }
                    }
                }
            }
        }
    }

    pub(crate) async fn resolve_fee_params(
        &self,
        fee_config: L1SenderFeeConfig,
        force_transaction_resubmission: bool,
    ) -> anyhow::Result<FeeParams> {
        if force_transaction_resubmission {
            return Ok(fee_config.replacement_fee_params());
        }

        let configured_params = fee_config.configured_fee_params();
        let estimated = self.provider.estimate_eip1559_fees().await?;
        L1_SENDER_METRICS.report_l1_eip_1559_estimation(estimated)?;

        tracing::debug!(
            max_priority_fee_per_gas_gwei = ?format_units(estimated.max_priority_fee_per_gas, "gwei"),
            max_fee_per_gas_gwei = ?format_units(estimated.max_fee_per_gas, "gwei"),
            "estimated priority and max fees"
        );

        Ok(apply_fee_caps(configured_params, estimated))
    }

    /// Estimates gas limits for a batch of L1 commands via `eth_simulateV1`, returning
    /// `2 * gas_used` per call. Each command goes into its own simulated block so
    /// cumulative block-gas-limit constraints can't reject the batch, while writes from
    /// earlier blocks remain visible to later ones (spec-mandated overlay propagation).
    /// Falls back to [`L1_GAS_LIMIT_FALLBACK`] per tx on any simulate failure.
    ///
    /// `in_flight_prefix` mirrors already-submitted-but-unmined transactions ahead of
    /// `commands` in the payload so chained calls (batch N+1 requires batch N committed) see
    /// the right state on providers whose simulation base excludes pool transactions. On
    /// providers whose base already includes them, the prefix call reverts without applying
    /// state — harmless either way (see [`SimPrefixEntry`]); prefix results are discarded.
    pub(crate) async fn estimate_gas_limits(
        &self,
        in_flight_prefix: &[SimPrefixEntry],
        commands: &[Input],
        prepared_sidecars: &[Option<Arc<PreparedSidecar>>],
        operator_address: Address,
        fee_params: FeeParams,
        // Sequential nonces start here — anvil's EIP-4844 parsing requires `nonce` and
        // `gas_limit` even with `validation=false`. Matches the send nonces.
        starting_nonce: u64,
    ) -> anyhow::Result<Vec<u64>> {
        // eth_simulateV1 caps a payload at 256 blocks; with default configs
        // prefix+commands is ≤ 32, so hitting this means a misconfiguration — degrade to
        // un-prefixed estimation rather than fail.
        let in_flight_prefix = if in_flight_prefix.len() + commands.len() > 256 {
            tracing::warn!(
                prefix_len = in_flight_prefix.len(),
                commands_len = commands.len(),
                "in-flight prefix + commands exceed the eth_simulateV1 256-block cap; \
                 estimating without the in-flight prefix",
            );
            &[]
        } else {
            in_flight_prefix
        };

        // Some L1 providers check sender balance even with `validation=false`; override
        // to bypass.
        let balance_override = StateOverridesBuilder::default()
            .append(
                operator_address,
                AccountOverride {
                    balance: Some(U256::MAX),
                    ..Default::default()
                },
            )
            .build();

        let prefix_blocks = in_flight_prefix.iter().map(|entry| {
            build_l1_simulation_request(
                operator_address,
                self.to_address,
                entry.calldata.clone(),
                entry.nonce,
                entry.fee_params,
                entry.sidecar.as_deref(),
            )
        });
        let command_blocks = commands.iter().enumerate().map(|(i, cmd)| {
            build_l1_simulation_request(
                operator_address,
                self.to_address,
                cmd.solidity_call(&operator_address),
                starting_nonce + i as u64,
                fee_params,
                prepared_sidecars[i].as_deref(),
            )
        });
        let block_state_calls = prefix_blocks
            .chain(command_blocks)
            .map(|req| {
                let mut sim_block = SimBlock::default().call(req);
                sim_block.state_overrides = Some(balance_override.clone());
                sim_block
            })
            .collect::<Vec<_>>();

        let expected_blocks = block_state_calls.len();
        let payload = SimulatePayload {
            block_state_calls,
            ..Default::default()
        };

        // Top-level failures fall back across the batch; per-call reverts fall back only
        // for that tx.
        let blocks = match self.provider.simulate(&payload).pending().await {
            Ok(blocks) if blocks.len() == expected_blocks => blocks,
            Ok(blocks) => {
                tracing::warn!(
                    returned = blocks.len(),
                    expected = expected_blocks,
                    "eth_simulateV1 returned mismatched block count, falling back to {L1_GAS_LIMIT_FALLBACK} per tx",
                );
                return Ok(vec![L1_GAS_LIMIT_FALLBACK; commands.len()]);
            }
            Err(err) => {
                tracing::warn!(
                    %err,
                    "eth_simulateV1 unavailable or errored, falling back to {L1_GAS_LIMIT_FALLBACK} per tx",
                );
                return Ok(vec![L1_GAS_LIMIT_FALLBACK; commands.len()]);
            }
        };

        let gas_limits = blocks
            .iter()
            // A prefix call reverting is expected on providers whose simulation base already
            // includes pool transactions; only the trailing per-command results matter.
            .skip(in_flight_prefix.len())
            .enumerate()
            .map(|(i, block)| match block.calls.first() {
                Some(call) if call.status => call.gas_used.saturating_mul(2),
                Some(call) => {
                    tracing::warn!(
                        tx_index = i,
                        return_data = ?call.return_data,
                        "eth_simulateV1 call reverted, falling back to {L1_GAS_LIMIT_FALLBACK}",
                    );
                    L1_GAS_LIMIT_FALLBACK
                }
                None => {
                    tracing::warn!(
                        tx_index = i,
                        "eth_simulateV1 block had no call result, falling back to {L1_GAS_LIMIT_FALLBACK}",
                    );
                    L1_GAS_LIMIT_FALLBACK
                }
            })
            .collect();
        Ok(gas_limits)
    }

    async fn report_custom_priority_fee_metrics(&self) -> anyhow::Result<()> {
        for (window, blocks_behind) in [
            (PriorityFeeEstimateWindow::Blocks3, 3),
            (PriorityFeeEstimateWindow::Blocks5, 5),
            (PriorityFeeEstimateWindow::Blocks10, 10),
        ] {
            for (percentile_label, percentile) in [
                (PriorityFeeEstimatePercentile::P20, 20.0),
                (PriorityFeeEstimatePercentile::P30, 30.0),
                (PriorityFeeEstimatePercentile::P50, 50.0),
            ] {
                let our_eip1559_est = self
                    .estimate_eip1559_fees(blocks_behind, percentile)
                    .await?;
                L1_SENDER_METRICS.report_custom_estimated_max_priority_fee_per_gas(
                    window,
                    percentile_label,
                    our_eip1559_est.max_priority_fee_per_gas,
                )?;
            }
        }
        Ok(())
    }

    async fn report_operator_metrics_loop(&self) -> anyhow::Result<()> {
        let command_name = Input::COMPONENT_ID.as_str();
        let mut timer = tokio::time::interval(OPERATOR_METRICS_POLL_INTERVAL);
        loop {
            timer.tick().await;
            let operator_address = self.operator_address().await?;
            let balance = format_ether(self.provider.get_balance(operator_address).await?);
            let nonce = self
                .provider
                .get_transaction_count(operator_address)
                .await?;
            L1_SENDER_METRICS.balance[&command_name].set(balance.parse()?);
            L1_SENDER_METRICS.nonce[&command_name].set(nonce);
            // Dashboard-only estimates; a failed poll must not take the sender down.
            if let Err(err) = self.report_custom_priority_fee_metrics().await {
                tracing::warn!(
                    command_name,
                    %err,
                    "failed to report priority-fee estimate metrics"
                );
            }
        }
    }

    /// Estimates EIP-1559 fees using the provided percentile of priority fees over the specified
    /// fee-history window.
    ///
    /// `estimate_eip1559_fees_with` in alloy hardcodes the block count and percentile, so we call
    /// `get_fee_history` directly and delegate the rest to alloy's default estimator.
    async fn estimate_eip1559_fees(
        &self,
        blocks_behind: u64,
        percentile: f64,
    ) -> anyhow::Result<Eip1559Estimation> {
        let fee_history = self
            .provider
            .get_fee_history(blocks_behind, BlockNumberOrTag::Latest, &[percentile])
            .await
            .context("fetching fee history")?;
        let base_fee_per_gas: u128 = fee_history.latest_block_base_fee().unwrap_or_default();
        let rewards = fee_history.reward.unwrap_or_default();
        Ok(alloy::providers::utils::eip1559_default_estimator(
            base_fee_per_gas,
            &rewards,
        ))
    }

    async fn register_operator(&mut self) -> anyhow::Result<()> {
        let address = self
            .config
            .operator_signer
            .register_with_wallet(self.provider.wallet_mut())
            .await?;

        let balance = self.provider.get_balance(address).await?;
        let address_string: &'static str = address.to_string().leak();
        L1_SENDER_METRICS.l1_operator_address[&(Input::COMPONENT_ID.as_str(), address_string)]
            .set(1);

        if balance.is_zero() {
            anyhow::bail!("L1 sender's address {address} has zero balance");
        }

        tracing::info!(
            command_name = Input::COMPONENT_ID.as_str(),
            balance_eth = format_ether(balance),
            %address,
            "initialized L1 sender",
        );
        Ok(())
    }

    pub(crate) async fn validate_tx_receipt(
        &self,
        command: &Input,
        receipt: TransactionReceipt,
    ) -> anyhow::Result<()> {
        let execution_fee = receipt.gas_used as u128 * receipt.effective_gas_price;
        let blob_fee = receipt
            .blob_gas_used
            .zip(receipt.blob_gas_price)
            .map(|(gas_used, gas_price)| gas_used as u128 * gas_price)
            .unwrap_or_default();
        let balance_consumed = execution_fee
            .saturating_add(blob_fee)
            .min(u128::from(u64::MAX)) as u64;

        L1_SENDER_METRICS.balance_consumed_by_tx[&Input::COMPONENT_ID.as_str()]
            .set(balance_consumed);

        if receipt.status() {
            // Transaction succeeded - log output and return OK(())
            L1_SENDER_METRICS.report_tx_receipt(command, receipt)?;
            Ok(())
        } else {
            tracing::error!(
                %command,
                tx_hash = ?receipt.transaction_hash,
                l1_block_number = receipt.block_number.unwrap(),
                "Transaction failed on L1",
            );
            if let Ok(trace) = self
                .provider
                .debug_trace_transaction(
                    receipt.transaction_hash,
                    GethDebugTracingOptions::call_tracer(CallConfig::default()),
                )
                .await
            {
                let call_frame = trace
                    .try_into_call_frame()
                    .expect("requested call tracer but received a different call frame type");
                // We print top-level call frame's output as it likely contains serialized custom
                // error pointing to the underlying problem (i.e. starts with the error's 4byte
                // signature).
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
    }
}

/// Nonce-class `eth_sendRawTransaction` rejections.
fn is_nonce_error(err: &TransportError) -> bool {
    match err {
        TransportError::ErrorResp(payload) => {
            let message = payload.message.to_lowercase();
            message.contains("nonce too low")
                || message.contains("nonce too high")
                || message.contains("nonce gap")
        }
        _ => false,
    }
}

/// Max submission attempts when the L1 node rejects a transaction because the tx pool is at
/// capacity. Sized to outlast several L1 slots — pool capacity frees as blocks mine, e.g.
/// after a reorg briefly returns already-mined transactions to the pool while the in-flight
/// window is full.
const POOL_CAPACITY_ERROR_MAX_ATTEMPTS: usize = 30;

/// Fee-class `eth_sendRawTransaction` rejections: the transaction is priced below what the
/// node currently accepts (base fee above our configured cap, or the pool's dynamic price
/// floor rose). The fee caps are a deliberate operator bound, so the sender must WAIT for the
/// market to come back under them — stalling (and letting backpressure engage) rather than
/// crash-looping through restarts for the duration of a fee spike.
///
/// Replacement underpricing is deliberately NOT in this class: waiting never fixes an RBF
/// bump that is too small, so it stays fatal.
fn is_fee_too_low_error(err: &TransportError) -> bool {
    match err {
        TransportError::ErrorResp(payload) => {
            let message = payload.message.to_lowercase();
            message.contains("less than block base fee")
                || message.contains("less than blob base fee")
                || (message.contains("underpriced") && !message.contains("replacement"))
        }
        _ => false,
    }
}

/// Pool-capacity-class `eth_sendRawTransaction` rejections: the sender's per-account slot
/// limit (geth blobpool `maxTxsPerAccount`, reth `max-account-slots`) or the global pool
/// capacity is exhausted. Unlike nonce errors these clear as L1 mines blocks, so they get a
/// longer retry budget ([`POOL_CAPACITY_ERROR_MAX_ATTEMPTS`]).
fn is_pool_capacity_error(err: &TransportError) -> bool {
    match err {
        TransportError::ErrorResp(payload) => {
            let message = payload.message.to_lowercase();
            message.contains("account limit exceeded")
                || message.contains("account slots")
                || message.contains("txpool is full")
                || message.contains("pool is full")
                || message.contains("in-flight transaction limit")
                || message.contains("too many pending transactions")
        }
        _ => false,
    }
}

/// Combines operator-configured fee caps with the network's EIP-1559 estimate.
///
/// `max_fee_per_gas` and `max_fee_per_blob_gas` are taken verbatim from
/// `configured` — they are static caps set by the operator and never adjusted
/// up from network estimates. Only `max_priority_fee_per_gas` follows the
/// estimate, capped from above by the configured value.
fn apply_fee_caps(configured: FeeParams, estimated: Eip1559Estimation) -> FeeParams {
    if estimated.max_fee_per_gas > configured.max_fee_per_gas {
        tracing::warn!(
            "L1 sender's configured maxFeePerGas ({}) \
             is lower than the one estimated from network  ({}), \
             using the configured base fee value ({}) - this may result in inclusion delay.",
            configured.max_fee_per_gas,
            estimated.max_fee_per_gas,
            configured.max_fee_per_gas,
        );
    }

    let max_priority_fee_per_gas =
        if estimated.max_priority_fee_per_gas > configured.max_priority_fee_per_gas {
            tracing::warn!(
                "L1 sender's configured max_priority_fee_per_gas ({}) \
             is lower than the one estimated from network  ({}), \
             using the configured priority fee value ({}) - this may result in inclusion delay.",
                configured.max_priority_fee_per_gas,
                estimated.max_priority_fee_per_gas,
                configured.max_priority_fee_per_gas,
            );
            configured.max_priority_fee_per_gas
        } else {
            estimated.max_priority_fee_per_gas
        };

    FeeParams {
        max_fee_per_gas: configured.max_fee_per_gas,
        max_priority_fee_per_gas,
        max_fee_per_blob_gas: configured.max_fee_per_blob_gas,
    }
}

impl L1SenderFeeConfig {
    fn configured_fee_params(self) -> FeeParams {
        FeeParams {
            max_fee_per_gas: self.max_fee_per_gas_wei,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas_wei,
            max_fee_per_blob_gas: self.max_fee_per_blob_gas_wei,
        }
    }

    fn replacement_fee_params(self) -> FeeParams {
        FeeParams {
            max_fee_per_gas: ((self.max_fee_per_gas_wei as f64)
                * self.max_fee_per_gas_replacement_multiplier)
                .ceil() as u128,
            max_priority_fee_per_gas: ((self.max_priority_fee_per_gas_wei as f64)
                * self.max_priority_fee_per_gas_replacement_multiplier)
                .ceil() as u128,
            max_fee_per_blob_gas: ((self.max_fee_per_blob_gas_wei as f64)
                * self.max_fee_per_blob_gas_replacement_multiplier)
                .ceil() as u128,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_error_classification() {
        use alloy::rpc::json_rpc::ErrorPayload;
        use alloy::transports::TransportErrorKind;

        let resp = |message: &str| {
            TransportError::ErrorResp(ErrorPayload {
                code: -32000,
                message: message.to_string().into(),
                data: None,
            })
        };

        // reth rejects a nonce-gapped blob transaction with exactly "nonce too high"; geth
        // appends details after the same prefix; some clients capitalize.
        assert!(is_nonce_error(&resp("nonce too high")));
        assert!(is_nonce_error(&resp(
            "nonce too high: tx nonce 7, gapped nonce 5"
        )));
        assert!(is_nonce_error(&resp(
            "nonce too low: next nonce 5, tx nonce 3"
        )));
        assert!(is_nonce_error(&resp("Nonce too high")));
        assert!(is_nonce_error(&resp("nonce gap for sender")));

        // Non-nonce rejections and transport-level failures are not retryable here: a
        // transport failure is ambiguous (the tx may have been admitted), so it must
        // propagate rather than trigger a re-send.
        assert!(!is_nonce_error(&resp(
            "insufficient funds for gas * price + value"
        )));
        assert!(!is_nonce_error(&resp(
            "replacement transaction underpriced"
        )));
        assert!(!is_nonce_error(&TransportErrorKind::custom_str(
            "error sending request"
        )));
    }

    #[test]
    fn blob_simulation_request_is_buildable_only_with_sidecar() {
        let operator_address = Address::with_last_byte(1);
        let to_address = Address::with_last_byte(2);
        let input = Bytes::from_static(b"commit");
        let nonce = 7;
        let fee_params = FeeParams {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            max_fee_per_blob_gas: 20,
        };
        let blob_sidecar = alloy::consensus::BlobTransactionSidecar::default();

        let mut old_request = TransactionRequest::default()
            .with_from(operator_address)
            .with_to(to_address)
            .with_input(input.clone())
            .with_max_fee_per_gas(fee_params.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fee_params.max_priority_fee_per_gas)
            .with_nonce(nonce)
            .with_gas_limit(L1_SIM_GAS_LIMIT);
        old_request.blob_versioned_hashes = Some(blob_sidecar.versioned_hashes().collect());
        old_request.max_fee_per_blob_gas = Some(fee_params.max_fee_per_blob_gas);
        old_request.transaction_type = Some(3);

        let old_err = old_request
            .build_typed_simulate_transaction()
            .expect_err("hashes-only blob simulation request should not be buildable");
        assert!(
            old_err.to_string().contains("Transaction is not buildable"),
            "unexpected old request error: {old_err}",
        );

        let prepared = PreparedSidecar {
            blob_count: blob_sidecar.blobs.len() as u64,
            variant: BlobTransactionSidecarVariant::Eip4844(blob_sidecar),
        };
        let fixed_request = build_l1_simulation_request(
            operator_address,
            to_address,
            input,
            nonce,
            fee_params,
            Some(&prepared),
        );
        fixed_request
            .build_typed_simulate_transaction()
            .expect("sidecar-backed blob simulation request should be buildable");
    }

    #[test]
    fn pool_capacity_error_classification() {
        use alloy::rpc::json_rpc::ErrorPayload;
        use alloy::transports::TransportErrorKind;

        let resp = |message: &str| {
            TransportError::ErrorResp(ErrorPayload {
                code: -32000,
                message: message.to_string().into(),
                data: None,
            })
        };

        // geth blobpool per-account cap and legacy-pool global capacity.
        assert!(is_pool_capacity_error(&resp(
            "account limit exceeded: pooled 16 txs"
        )));
        assert!(is_pool_capacity_error(&resp("txpool is full")));
        // reth per-account slot cap phrasing.
        assert!(is_pool_capacity_error(&resp(
            "rejected due to account slots limit"
        )));
        assert!(is_pool_capacity_error(&resp(
            "Too many pending transactions for sender"
        )));

        // Not capacity-class: nonce errors, replacement pricing, transport failures.
        assert!(!is_pool_capacity_error(&resp("nonce too high")));
        assert!(!is_pool_capacity_error(&resp(
            "replacement transaction underpriced"
        )));
        assert!(!is_pool_capacity_error(&resp("already known")));
        assert!(!is_pool_capacity_error(&TransportErrorKind::custom_str(
            "error sending request"
        )));
    }

    #[test]
    fn fee_too_low_error_classification() {
        use alloy::rpc::json_rpc::ErrorPayload;
        use alloy::transports::TransportErrorKind;

        let resp = |message: &str| {
            TransportError::ErrorResp(ErrorPayload {
                code: -32003,
                message: message.to_string().into(),
                data: None,
            })
        };

        // anvil rejects submissions priced under the next block's base fee outright; geth
        // rejects under the pool's (dynamic) price floor with "transaction underpriced".
        assert!(is_fee_too_low_error(&resp(
            "max fee per gas less than block base fee"
        )));
        assert!(is_fee_too_low_error(&resp(
            "max fee per blob gas less than blob base fee"
        )));
        assert!(is_fee_too_low_error(&resp("transaction underpriced")));

        // An RBF bump that is too small never resolves by waiting — must stay fatal.
        assert!(!is_fee_too_low_error(&resp(
            "replacement transaction underpriced"
        )));
        assert!(!is_fee_too_low_error(&resp("nonce too low")));
        assert!(!is_fee_too_low_error(&TransportErrorKind::custom_str(
            "error sending request"
        )));
    }

    /// `max_fee_per_gas` and `max_fee_per_blob_gas` are static caps set by
    /// the operator — they must equal the configured values regardless of
    /// what the network estimate reports. Only `max_priority_fee_per_gas` is
    /// allowed to track the estimate (capped from above).
    #[test]
    fn apply_fee_caps_keeps_max_fee_and_blob_fee_static() {
        let configured = FeeParams {
            max_fee_per_gas: 100_000_000_000,
            max_priority_fee_per_gas: 2_000_000_000,
            max_fee_per_blob_gas: 50_000_000_000,
        };

        // Estimates spanning far below, equal to, and far above the configured
        // caps — the static fields must stay pinned to the configured values
        // in every case.
        let cases = [
            Eip1559Estimation {
                max_fee_per_gas: 1,
                max_priority_fee_per_gas: 1,
            },
            Eip1559Estimation {
                max_fee_per_gas: configured.max_fee_per_gas,
                max_priority_fee_per_gas: configured.max_priority_fee_per_gas,
            },
            Eip1559Estimation {
                max_fee_per_gas: configured.max_fee_per_gas * 10,
                max_priority_fee_per_gas: configured.max_priority_fee_per_gas * 10,
            },
        ];

        for est in cases {
            let capped = apply_fee_caps(configured, est);
            assert_eq!(
                capped.max_fee_per_gas, configured.max_fee_per_gas,
                "max_fee_per_gas must equal configured cap (estimate: {est:?})",
            );
            assert_eq!(
                capped.max_fee_per_blob_gas, configured.max_fee_per_blob_gas,
                "max_fee_per_blob_gas must equal configured cap (estimate: {est:?})",
            );
            assert!(
                capped.max_priority_fee_per_gas <= configured.max_priority_fee_per_gas,
                "max_priority_fee_per_gas must never exceed configured cap \
                 (got {}, cap {}, estimate: {est:?})",
                capped.max_priority_fee_per_gas,
                configured.max_priority_fee_per_gas,
            );
        }
    }
}
