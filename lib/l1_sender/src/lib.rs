pub mod batcher_metrics;
pub mod batcher_model;
pub mod commands;
pub mod config;
mod error;
mod metrics;
pub mod pipeline_component;
mod submitter;
mod types;
pub mod upgrade_gatekeeper;
mod watcher;

pub(crate) use submitter::Submitter;
pub(crate) use watcher::Watcher;

use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::Address;
use alloy::primitives::utils::format_ether;
use alloy::providers::ext::DebugApi;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{Provider, WalletProvider};
use alloy::rpc::types::TransactionReceipt;
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use anyhow::Context;
use tokio::sync::mpsc::Sender;
use zksync_os_observability::ComponentStateReporter;
use zksync_os_operator_signer::SignerConfig;
use zksync_os_pipeline::PeekableReceiver;

/// Process responsible for sending transactions to L1.
/// Handles one type of l1 command (e.g. Commit or Prove).
/// Loads up to `command_limit` commands from the channel and sends them to L1 in parallel.
/// Waits for all transactions to be mined, sends them to the output channel
/// and then starts with the next `command_limit` commands.
///
/// Important: the same provider (sender address) must not be used outside this process.
///     Otherwise, there will be a nonce conflict and a failed L1 transaction
///     (recoverable on restart)
///
/// Note: we pass `to_address` - L1 contract address to send transactions to.
/// It differs between commit/prove/execute (e.g., timelock vs diamond proxy)
pub async fn run_l1_sender<Input: SendToL1 + Send + 'static>(
    inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: Sender<SignedBatchEnvelope<FriProof>>,
    to_address: Address,
    mut provider: FillProvider<
        impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
        impl Provider<Ethereum> + Clone,
    >,
    config: L1SenderConfig<Input>,
    gateway: bool,
) -> anyhow::Result<()> {
    let command_name = Input::NAME;
    let latency_tracker =
        ComponentStateReporter::global().handle_for(command_name, L1SenderState::WaitingRecv);

    let (operator_address, next_nonce) =
        register_operator::<_, Input>(&mut provider, config.operator_signer.clone()).await?;

    // Capacity = command_limit: the Submitter can get at most `command_limit`
    // items ahead of the Watcher before backpressure kicks in.
    let (in_flight_tx, in_flight_rx) = tokio::sync::mpsc::channel(config.command_limit);
    // Capacity = 1: the Watcher can queue at most one resubmit request; the
    // Submitter must handle it before the Watcher proceeds.
    let (resubmit_tx, resubmit_rx) = tokio::sync::mpsc::channel(1);

    // Extract Watcher-specific config fields before moving config into Submitter.
    let poll_interval = config.poll_interval;
    let transaction_timeout = config.transaction_timeout;

    let submitter = Submitter {
        inbound,
        in_flight_tx,
        resubmit_rx,
        to_address,
        provider: provider.clone(),
        config,
        gateway,
        operator_address,
        next_nonce,
        latency_tracker: latency_tracker.clone(),
    };

    let watcher = Watcher {
        in_flight_rx,
        resubmit_tx,
        outbound,
        provider,
        operator_address,
        poll_interval,
        transaction_timeout,
        latency_tracker,
    };

    tokio::try_join!(submitter.run(), watcher.run())?;
    Ok(())
}

async fn register_operator<
    P: Provider + WalletProvider<Wallet = EthereumWallet>,
    Input: SendToL1,
>(
    provider: &mut P,
    signer_config: SignerConfig,
) -> anyhow::Result<(Address, u64)> {
    let address = signer_config
        .register_with_wallet(provider.wallet_mut())
        .await?;

    let balance = provider
        .get_balance(address)
        .await
        .context("fetching operator balance")?;
    L1_SENDER_METRICS.balance[&Input::NAME].set(format_ether(balance).parse()?);
    let address_string: &'static str = address.to_string().leak();
    L1_SENDER_METRICS.l1_operator_address[&(Input::NAME, address_string)].set(1);

    if balance.is_zero() {
        anyhow::bail!("L1 sender's address {address} has zero balance");
    }

    let nonce = provider
        .get_transaction_count(address)
        .await
        .context("fetching operator nonce")?;
    L1_SENDER_METRICS.nonce[&Input::NAME].set(nonce);

    tracing::info!(
        command_name = Input::NAME,
        balance_eth = format_ether(balance),
        nonce,
        %address,
        "initialized L1 sender",
    );
    Ok((address, nonce))
}

pub(crate) async fn validate_tx_receipt<Input: SendToL1>(
    provider: &impl Provider,
    command: &Input,
    receipt: TransactionReceipt,
) -> anyhow::Result<()> {
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
