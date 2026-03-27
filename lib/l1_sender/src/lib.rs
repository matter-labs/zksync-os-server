pub mod batcher_metrics;
pub mod batcher_model;
pub mod commands;
pub mod config;
pub mod error;
mod metrics;
pub mod pipeline_component;
mod submitter;
mod watcher;
pub mod upgrade_gatekeeper;

pub use error::{L1SendError, RecoverableReason};

use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use crate::submitter::Submitter;
use crate::watcher::Watcher;
use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, B256};
use alloy::providers::ext::DebugApi;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{PendingTransactionError, Provider, WalletProvider};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::TransactionReceipt;
use futures::future::BoxFuture;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_observability::{ComponentStateHandle, ComponentStateReporter};
use zksync_os_operator_signer::SignerConfig;
use zksync_os_pipeline::PeekableReceiver;

/// Maximum time to wait for a transaction to be included on L1.
///
/// Normally 15-30 seconds is enough for normal priority transactions, and 60-120 is enough for
/// lower gas price transactions. We picked 300 seconds conservatively as it should cover most
/// scenarios with network congestion.
pub(crate) const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(300);

/// Future that resolves into a (fallible) transaction receipt.
pub(crate) type TransactionReceiptFuture =
    BoxFuture<'static, Result<TransactionReceipt, PendingTransactionError>>;

// ==============================================================================
// Exponential Backoff
// ==============================================================================

/// Simple exponential backoff with a configurable initial delay, multiplier, and cap.
///
/// Used to pace retries after transient and recoverable errors so we don't hammer
/// a struggling RPC endpoint.
pub(crate) struct ExponentialBackoff {
    initial: Duration,
    current: Duration,
    max: Duration,
}

impl ExponentialBackoff {
    pub(crate) fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            current: initial,
            max,
        }
    }

    /// Returns the current delay and doubles it (capped at `max`) for the next call.
    pub(crate) fn next(&mut self) -> Duration {
        let delay = self.current;
        self.current = std::cmp::min(self.current * 2, self.max);
        delay
    }

    /// Resets the delay to the initial value after a successful cycle.
    pub(crate) fn reset(&mut self) {
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
pub(crate) struct InFlightTx<Input> {
    pub(crate) command: Input,
    pub(crate) tx_hash: B256,
    pub(crate) receipt_future: TransactionReceiptFuture,
}

// ==============================================================================
// Public entry point
// ==============================================================================

/// Runs the L1 sender for one command type (commit, prove, or execute).
///
/// Handles operator registration and passthrough commands before spawning the
/// Submitter and Watcher tasks under `tokio::try_join!`.
pub async fn run_l1_sender<Input: SendToL1 + Send + 'static>(
    mut inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
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

    if process_prepending_passthrough_commands(
        &mut inbound,
        &outbound,
        &latency_tracker,
        Input::NAME,
    )
    .await?
    .is_none()
    {
        tracing::info!(
            command_name = Input::NAME,
            "inbound channel closed during passthrough phase"
        );
        return Ok(());
    }

    let channel_capacity = config.command_limit;
    let cmd_buffer_capacity = config.command_limit;

    let (in_flight_tx, in_flight_rx) = mpsc::channel(channel_capacity);
    let (resubmit_tx, resubmit_rx) = mpsc::channel(channel_capacity);

    // The Watcher gets a provider clone only for debug-tracing reverted txs.
    let watcher_provider = provider.clone();

    let submitter = Submitter {
        inbound,
        resubmit_rx,
        in_flight_tx,
        provider,
        config,
        to_address,
        operator_address,
        gateway,
        pending_commands: Vec::new(),
        latency_tracker: latency_tracker.clone(),
        backoff: ExponentialBackoff::new(Duration::from_secs(5), Duration::from_secs(60)),
        cmd_buffer: Vec::with_capacity(cmd_buffer_capacity),
    };

    let watcher = Watcher {
        in_flight_rx,
        resubmit_tx,
        outbound,
        provider: watcher_provider,
        latency_tracker,
    };

    tokio::try_join!(submitter.run(), watcher.run())?;
    Ok(())
}

// ==============================================================================
// Helper free functions (shared between submitter.rs, watcher.rs, and lib.rs)
// ==============================================================================

/// Converts a `RecoverableReason` to the Prometheus label string used by
/// `L1_SENDER_METRICS.recoverable_errors`.
pub(crate) fn reason_label(reason: RecoverableReason) -> &'static str {
    match reason {
        RecoverableReason::GasBlocked => "gas_blocked",
        RecoverableReason::BlobFeeBlocked => "blob_fee_blocked",
        RecoverableReason::TxTimeout => "tx_timeout",
        RecoverableReason::NonceTooLow => "nonce_too_low",
    }
}

/// Reports operator balance and nonce after a successful send cycle.
///
/// These calls are informational — RPC failures are logged at WARN level
/// and do not affect the send loop.
pub(crate) async fn report_balance_and_nonce<P, Input>(provider: &P, operator_address: Address)
where
    P: Provider<Ethereum>,
    Input: SendToL1,
{
    match provider.get_balance(operator_address).await {
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

    match provider.get_transaction_count(operator_address).await {
        Ok(nonce) => {
            L1_SENDER_METRICS.nonce[&Input::NAME].set(nonce);
        }
        Err(e) => tracing::warn!(?e, "failed to fetch operator nonce"),
    }
}

async fn process_prepending_passthrough_commands<Input: SendToL1>(
    inbound: &mut PeekableReceiver<L1SenderCommand<Input>>,
    outbound: &mpsc::Sender<SignedBatchEnvelope<FriProof>>,
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
pub(crate) async fn validate_tx_receipt_reverted<Input: SendToL1>(
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
