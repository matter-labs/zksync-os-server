pub mod batcher_metrics;
pub mod batcher_model;
pub mod commands;
pub mod config;
mod metrics;
pub mod pipeline_component;
pub mod upgrade_gatekeeper;

use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use alloy::consensus::{BlobTransactionValidationError, EnvKzgSettings};
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::Address;
use alloy::primitives::utils::{format_ether, format_units};
use alloy::providers::ext::DebugApi;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{PendingTransactionError, Provider, WalletProvider};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::signers::k256::ecdsa::SigningKey;
use alloy::signers::local::PrivateKeySigner;
use anyhow::Context;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use zksync_os_observability::{ComponentStateHandle, ComponentStateReporter};
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
/// Known issues:
///   * Crashes when there is a gap in incoming L1 blocks (happens periodically with Infura provider)
///   * Does not attempt to detect in-flight L1 transactions on startup - just crashes if they get mined
///
/// Note: we pass `to_address` - L1 contract address to send transactions to.
/// It differs between commit/prove/execute (e.g., timelock vs diamond proxy)
pub async fn run_l1_sender<Input: SendToL1>(
    // == plumbing ==
    mut inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: Sender<SignedBatchEnvelope<FriProof>>,

    // == command-specific settings ==
    to_address: Address,

    // == config ==
    mut provider: FillProvider<
        impl TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
        impl Provider<Ethereum>,
    >,
    config: L1SenderConfig<Input>,
) -> anyhow::Result<()> {
    let latency_tracker =
        ComponentStateReporter::global().handle_for(Input::NAME, L1SenderState::WaitingRecv);
    let command_name = Input::NAME;

    let operator_address = register_operator::<_, Input>(&mut provider, config.operator_sk).await?;
    let mut cmd_buffer = Vec::with_capacity(config.command_limit);

    // Process all potential passthrough commands first
    process_prepending_passthrough_commands(
        &mut inbound,
        &outbound,
        &latency_tracker,
        command_name,
    )
    .await?;
    // At this point, only actual SendToL1 commands are expected
    loop {
        latency_tracker.enter_state(L1SenderState::WaitingRecv);
        // This sleeps until **at least one** command is received from the channel. Additionally,
        // receives up to `self.command_limit` commands from the channel if they are ready (i.e. does
        // not wait for them). Extends `cmd_buffer` with received values and, as `cmd_buffer` is
        // emptied in every iteration, its size never exceeds `self.command_limit`.
        let received = inbound
            .recv_many(&mut cmd_buffer, config.command_limit)
            .await;
        let mut commands = cmd_buffer
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
        // This method only returns `0` if the channel has been closed and there are no more items
        // in the queue.
        if received == 0 {
            anyhow::bail!("inbound channel closed");
        }
        latency_tracker.enter_state(L1SenderState::SendingToL1);
        let range = Input::display_range(&commands); // Only for logging
        tracing::info!(command_name, range, "sending L1 transactions");
        L1_SENDER_METRICS.parallel_transactions[&command_name].set(cmd_buffer.len() as u64);
        // It's important to preserve the order of commands -
        // so that we send them downstream also in order.
        // This holds true because l1 transactions are included in the order of sender nonce.
        // Keep this in mind if changing sending logic (that is, if adding `buffer` we'd need to set nonce manually)
        let pending_txs: Vec<(TransactionReceiptFuture, Input)> =
            futures::stream::iter(commands.drain(..))
                .then(|mut cmd| async {
                    let mut tx_request = tx_request_with_gas_fields(
                        &provider,
                        operator_address,
                        config.max_fee_per_gas_wei,
                        config.max_priority_fee_per_gas_wei,
                    )
                    .await?
                    .with_to(to_address)
                    .with_call(&cmd.solidity_call());

                    if let Some(blob_sidecar) = cmd.blob_sidecar() {
                        let fee_per_blob_gas = provider.get_blob_base_fee().await?;
                        L1_SENDER_METRICS
                            .report_blob_base_fee(fee_per_blob_gas)?;
                        let max_fee_per_blob_gas = config.max_fee_per_blob_gas_wei;

                        if fee_per_blob_gas > max_fee_per_blob_gas {
                            tracing::warn!(
                                max_fee_per_blob_gas,
                                fee_per_blob_gas,
                                "L1 sender's configured maxFeePerBlobGas is lower than the one estimated from network"
                            );
                        }
                        tx_request.set_max_fee_per_blob_gas(max_fee_per_blob_gas);
                        tx_request.set_blob_sidecar(blob_sidecar);
                    };

                    // Fill the transaction (e.g., nonce, gas, etc.) using the provider and convert it to an
                    // envelope.
                    let envelope = provider.fill(tx_request).await?.try_into_envelope()?.try_into_pooled()?;

                    let pending_block = provider.get_block(BlockId::pending()).await?.expect("no pending block");
                    // todo: make conversion unconditional (and remove respective config) once both:
                    //       1) Fusaka upgrade is executed on mainnet
                    //       2) anvil supports EIP-7594 blobs (see https://github.com/foundry-rs/foundry/issues/12222)
                    let tx = if config.fusaka_upgrade_timestamp <= pending_block.header.timestamp {
                        // Convert the envelope into an EIP-7594 transaction by converting the sidecar
                        envelope.try_map_eip4844(|tx| {
                            tx.try_map_sidecar(|sidecar| {
                                Ok::<_, BlobTransactionValidationError>(
                                    BlobTransactionSidecarVariant::Eip7594(sidecar.try_into_7594(EnvKzgSettings::Default.get())?)
                                )
                            })
                        })?
                    } else {
                        // Keep the regular EIP-4844 sidecar
                        envelope.map_eip4844(|tx| tx.map_sidecar(BlobTransactionSidecarVariant::Eip4844))
                    };

                    // We don't wait for receipt here, instead we register an alloy watcher that
                    // polls for the receipt in the background. This future resolves when the watcher
                    // finds it.
                    let receipt_fut = provider
                        .send_raw_transaction(&tx.encoded_2718())
                        .await?
                        // We are being optimistic with our transaction inclusion here. But, even if
                        // reorg happens and transaction will not be included in the new fork (very-very
                        // unlikely), L1 sender will crash at some point (because a consequent L1
                        // transactions will fail) and recover from the new L1 state after restart.
                        .with_required_confirmations(1)
                        // Ensure we don't wait indefinitely and crash if the transaction is not
                        // included on L1 in a reasonable time.
                        .with_timeout(Some(TRANSACTION_TIMEOUT))
                        .get_receipt()
                        .boxed();
                    cmd.as_mut()
                        .iter_mut()
                        .for_each(|envelope| envelope.set_stage(Input::SENT_STAGE));
                    anyhow::Ok((receipt_fut, cmd))
                })
                // We could buffer the stream here to enable sending multiple batches of transactions in parallel,
                // but this is not necessary for now - we wait for them to be included in parallel
                .try_collect::<Vec<_>>()
                .await?;
        tracing::info!(command_name, range, "sent to L1, waiting for inclusion");
        latency_tracker.enter_state(L1SenderState::WaitingL1Inclusion);

        let mut completed_commands = Vec::with_capacity(pending_txs.len());
        for (receipt_fut, command) in pending_txs {
            let receipt = receipt_fut.await?;
            validate_tx_receipt(&provider, &command, receipt).await?;
            completed_commands.push(command);
        }

        let balance = format_ether(provider.get_balance(operator_address).await?);
        let nonce = provider.get_transaction_count(operator_address).await?;
        tracing::info!(
            command_name,
            range,
            balance,
            nonce,
            "all transactions included, sending downstream",
        );
        L1_SENDER_METRICS.balance[&command_name].set(balance.parse()?);
        L1_SENDER_METRICS.nonce[&command_name].set(nonce);
        latency_tracker.enter_state(L1SenderState::WaitingSend);
        for command in completed_commands {
            for mut output_envelope in command.into() {
                output_envelope.set_stage(Input::MINED_STAGE);
                outbound.send(output_envelope).await?;
            }
        }
    }
}

async fn process_prepending_passthrough_commands<Input: SendToL1>(
    inbound: &mut PeekableReceiver<L1SenderCommand<Input>>,
    outbound: &Sender<SignedBatchEnvelope<FriProof>>,
    latency_tracker: &ComponentStateHandle<L1SenderState>,
    command_name: &str,
) -> anyhow::Result<()> {
    loop {
        latency_tracker.enter_state(L1SenderState::WaitingRecv);
        match inbound
            .peek_recv(|command| matches!(command, L1SenderCommand::Passthrough(_)))
            .await
        {
            None => anyhow::bail!("inbound channel closed"),
            // command is SendToL1 (not passthrough)
            // we don't expect anymore passthroughs and can proceed with normal operations
            Some(false) => return Ok(()),
            // command is passthrough
            Some(true) => {
                let next_command = inbound.recv().await.context("Inbound channel closed")?;
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

async fn tx_request_with_gas_fields(
    provider: &dyn Provider,
    operator_address: Address,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> anyhow::Result<TransactionRequest> {
    let eip1559_est = provider.estimate_eip1559_fees().await?;
    L1_SENDER_METRICS.report_l1_eip_1559_estimation(eip1559_est)?;
    tracing::debug!(
        max_priority_fee_per_gas_gwei = ?format_units(eip1559_est.max_priority_fee_per_gas, "gwei"),
        max_fee_per_gas_gwei = ?format_units(eip1559_est.max_fee_per_gas, "gwei"),
        "estimated priority and max fees"
    );
    if eip1559_est.max_fee_per_gas > max_fee_per_gas {
        tracing::warn!(
            max_fee_per_gas = max_fee_per_gas,
            estimated_max_fee_per_gas = eip1559_est.max_fee_per_gas,
            "L1 sender's configured maxFeePerGas is lower than the one estimated from network"
        );
    }
    if eip1559_est.max_priority_fee_per_gas > max_priority_fee_per_gas {
        tracing::warn!(
            max_priority_fee_per_gas = max_priority_fee_per_gas,
            estimated_max_priority_fee_per_gas = eip1559_est.max_priority_fee_per_gas,
            "L1 sender's configured maxPriorityFeePerGas is lower than the one estimated from network"
        );
    }

    let tx = TransactionRequest::default()
        .with_from(operator_address)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
        // Default value for `max_aggregated_tx_gas` from zksync-era, should always be enough
        .with_gas_limit(15000000);
    Ok(tx)
}

async fn register_operator<
    P: Provider + WalletProvider<Wallet = EthereumWallet>,
    Input: SendToL1,
>(
    provider: &mut P,
    signing_key: SigningKey,
) -> anyhow::Result<Address> {
    let signer = PrivateKeySigner::from_signing_key(signing_key);
    let address = signer.address();
    provider.wallet_mut().register_signer(signer);

    let balance = provider.get_balance(address).await?;
    L1_SENDER_METRICS.balance[&Input::NAME].set(format_ether(balance).parse()?);
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

async fn validate_tx_receipt<Input: SendToL1>(
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
