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
use alloy::consensus::BlobTransactionValidationError;
use alloy::consensus::Transaction as ConsensusTransaction;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::{
    Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844, TransactionResponse,
};
use alloy::primitives::Address;
use alloy::primitives::utils::{format_ether, format_units};
use alloy::providers::ext::DebugApi;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{
    PendingTransactionBuilder, PendingTransactionError, Provider, WalletProvider,
};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::transports::TransportError;
use anyhow::Context;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::time::Instant;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;
use zksync_os_observability::{ComponentStateHandle, ComponentStateReporter};
use zksync_os_operator_signer::SignerConfig;
use zksync_os_pipeline::PeekableReceiver;

/// A code for "method not found" error response as declared in JSON-RPC 2.0 spec.
const METHOD_NOT_FOUND_CODE: i64 = -32601;
/// Estimated max amount of gas consumed by transaction sent by L1 sender is ~500k.
/// We set the limit higher to be safe.
const MAX_TX_GAS_USED: u64 = 2_000_000;
/// Number of L1 confirmations required before a transaction is considered final.
const REQUIRED_CONFIRMATIONS: u64 = 1;

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
///
/// Note: we pass `to_address` - L1 contract address to send transactions to.
/// It differs between commit/prove/execute (e.g., timelock vs diamond proxy)
#[allow(clippy::too_many_arguments)]
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
    gateway: bool,
    commit_submitted_tx: Option<watch::Sender<u64>>,
    // The SL block number at which `getTotalBatches*` was called on startup. Pinning the
    // confirmed-nonce baseline to this block ensures it is consistent with where the
    // inbound command queue begins — avoiding a crash caused by txs that are mined between
    // the `getTotalBatches` call and the nonce check.
    sl_block_number: u64,
) -> anyhow::Result<()> {
    let latency_tracker =
        ComponentStateReporter::global().handle_for(Input::NAME, L1SenderState::WaitingRecv);
    let command_name = Input::NAME;

    let operator_address =
        register_operator::<_, Input>(&mut provider, config.operator_signer).await?;
    let mut cmd_buffer = Vec::with_capacity(config.command_limit);
    // Process all potential passthrough commands first
    if process_prepending_passthrough_commands(
        &mut inbound,
        &outbound,
        &latency_tracker,
        command_name,
    )
    .await?
    .is_none()
    {
        tracing::info!("inbound channel closed");
        return Ok(());
    }

    // On startup, detect any L1 transactions that were submitted in a previous session
    // but not yet mined, and pair them with the corresponding queued commands.
    let recovered = match recover_in_flight_txs(
        &provider,
        operator_address,
        gateway,
        &mut inbound,
        command_name,
        sl_block_number,
    )
    .await
    {
        Ok(paired) => paired,
        Err(err) => {
            tracing::warn!("Error during in-flight transaction recovery: {err}");
            vec![]
        }
    };

    // Wait for any recovered in-flight transactions to be mined before accepting
    // new commands. Their nonces precede anything we are about to send, so they
    // must be confirmed first.
    if !recovered.is_empty() {
        let pending_txs: Vec<(TransactionReceiptFuture, Input, Instant)> = recovered
            .into_iter()
            .map(|(tx_hash, cmd)| {
                let fut = PendingTransactionBuilder::new(provider.root().clone(), tx_hash)
                    .with_required_confirmations(REQUIRED_CONFIRMATIONS)
                    .get_receipt()
                    .boxed();
                (fut, cmd, Instant::now())
            })
            .collect();
        wait_for_txs_and_forward(
            pending_txs,
            &provider,
            operator_address,
            command_name,
            &latency_tracker,
            &outbound,
        )
        .await?;
    }

    // At this point, all in-flight transactions from the previous session are confirmed.
    // Only actual SendToL1 commands are expected from here on.
    loop {
        latency_tracker.enter_state(L1SenderState::WaitingRecv);
        let received = inbound
            .recv_many(&mut cmd_buffer, config.command_limit)
            .await;

        if received == 0 {
            tracing::info!("inbound channel closed");
            return Ok(());
        }

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

        latency_tracker.enter_state(L1SenderState::SendingToL1);
        let range = Input::display_range(&commands); // Only for logging
        tracing::info!(command_name, range, "sending L1 transactions");
        L1_SENDER_METRICS.parallel_transactions[&command_name].set(commands.len() as u64);

        // It's important to preserve the order of commands so that we send them downstream
        // also in order. This holds because L1 transactions are included in sender-nonce
        // order. Keep this in mind if changing the sending logic (e.g., if adding a buffer
        // we'd need to set nonces manually).
        let pending_txs: Vec<(TransactionReceiptFuture, Input, Instant)> =
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
                    .with_input(cmd.solidity_call(gateway, &operator_address));

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
                    // todo: make conversion unconditional (and remove respective config) once anvil
                    //       supports EIP-7594 blobs (see https://github.com/foundry-rs/foundry/issues/12222)
                    let tx = if config.fusaka_upgrade_timestamp <= pending_block.header.timestamp {
                        // Convert the envelope into an EIP-7594 transaction by converting the sidecar
                        envelope.try_map_eip4844(|tx| {
                            tx.try_map_sidecar(|sidecar| {
                                Ok::<_, BlobTransactionValidationError>(
                                    BlobTransactionSidecarVariant::Eip7594(sidecar.try_into_eip7594()?)
                                )
                            })
                        })?
                    } else {
                        // Keep the regular EIP-4844 sidecar
                        envelope
                    };

                    // We don't wait for receipt here, instead we register an alloy watcher that
                    // polls for the receipt in the background. This future resolves when the watcher
                    // finds it.
                    let pending_tx = provider
                        .send_raw_transaction(&tx.encoded_2718())
                        .await?;
                    let submitted_at = Instant::now();
                    let pending_tx = pending_tx
                        // We are being optimistic with our transaction inclusion here. But, even if
                        // reorg happens and transaction will not be included in the new fork (very-very
                        // unlikely), L1 sender will crash at some point (because a consequent L1
                        // transactions will fail) and recover from the new L1 state after restart.
                        .with_required_confirmations(REQUIRED_CONFIRMATIONS)
                        // Ensure we don't wait indefinitely and crash if the transaction is not
                        // included on L1 in a reasonable time.
                        .with_timeout(Some(config.transaction_timeout));
                    let tx_hash = *pending_tx.tx_hash();
                    tracing::info!(
                        "{command_name}: L1 transaction submitted for {range}. Hash: {tx_hash:?} Waiting for inclusion...",
                    );
                    let receipt_fut = pending_tx.get_receipt().boxed();

                    // Notify CommitWatcher: this batch number has been submitted to L1.
                    if let Some(sender) = &commit_submitted_tx {
                        let batch_number = cmd
                            .as_ref()
                            .last()
                            .expect("commands is non-empty after recv_many")
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

                    cmd.as_mut()
                        .iter_mut()
                        .for_each(|envelope| envelope.set_stage(Input::SENT_STAGE));
                    anyhow::Ok((receipt_fut, cmd, submitted_at))
                })
                // We could buffer the stream here to enable sending multiple batches of transactions in parallel,
                // but this is not necessary for now - we wait for them to be included in parallel
                .try_collect::<Vec<_>>()
                .await?;
        tracing::info!(command_name, range, "sent to L1, waiting for inclusion");
        wait_for_txs_and_forward(
            pending_txs,
            &provider,
            operator_address,
            command_name,
            &latency_tracker,
            &outbound,
        )
        .await?;
    }
}

/// Waits for all pending L1 transaction receipts, validates them, logs balance/nonce
/// metrics, and forwards the completed commands downstream.
async fn wait_for_txs_and_forward<F, P, Input>(
    pending_txs: Vec<(TransactionReceiptFuture, Input, Instant)>,
    provider: &FillProvider<F, P>,
    operator_address: Address,
    command_name: &'static str,
    latency_tracker: &ComponentStateHandle<L1SenderState>,
    outbound: &Sender<SignedBatchEnvelope<FriProof>>,
) -> anyhow::Result<()>
where
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum>,
    Input: SendToL1,
{
    latency_tracker.enter_state(L1SenderState::WaitingL1Inclusion);

    let mut completed_commands = Vec::with_capacity(pending_txs.len());
    for (receipt_fut, command, submitted_at) in pending_txs {
        let receipt = receipt_fut.await;
        // Observe latency before propagating errors so timeout cases are recorded.
        L1_SENDER_METRICS.tx_inclusion_latency_seconds[&command_name]
            .observe(submitted_at.elapsed().as_secs_f64());
        let receipt = receipt?;
        validate_tx_receipt(provider, &command, receipt).await?;
        completed_commands.push(command);
    }

    let range = Input::display_range(&completed_commands);
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
    Ok(())
}

/// Detects in-flight L1 transactions from a previous session, pairs each one with the
/// corresponding queued command, and returns them ready to hand to the main loop.
///
/// For each in-flight tx, the next command is peeked and its calldata is compared against
/// the on-chain input. On a match the command is consumed and paired. On the first mismatch
/// the loop stops and whatever has been paired so far is returned — the unmatched command
/// remains in `inbound` for the normal send path.
///
/// `sl_block_number` must be the same L1 block at which `getTotalBatches*` was called when
/// constructing the inbound command queue. Pinning the confirmed-nonce baseline to that block
/// prevents the race where txs mined between the `getTotalBatches` call and this nonce check
/// cause us to mis-count in-flight txs and crash on calldata mismatch.
async fn recover_in_flight_txs<F, P, Input>(
    provider: &FillProvider<F, P>,
    operator_address: Address,
    gateway: bool,
    inbound: &mut PeekableReceiver<L1SenderCommand<Input>>,
    command_name: &str,
    sl_block_number: u64,
) -> anyhow::Result<Vec<(alloy::primitives::B256, Input)>>
where
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum>,
    Input: SendToL1,
{
    // Pin the confirmed-nonce to `sl_block_number` so it matches the snapshot at which
    // `getTotalBatches*` was evaluated and the inbound queue was initialised. Using the
    // "latest" block tag here would race with newly-mined blocks and produce a stale
    // baseline that is inconsistent with the queue's starting batch number.
    let latest_nonce = provider
        .get_transaction_count(operator_address)
        .block_id(BlockId::number(sl_block_number))
        .await
        .context("get confirmed transaction count")?;
    let pending_nonce = provider
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
        sl_block_number,
        latest_nonce,
        pending_nonce,
        in_flight_count,
        "Detected in-flight L1 transactions on startup, attempting recovery",
    );

    // Probe whether the provider supports `eth_getTransactionBySenderAndNonce` before
    // iterating over all pending nonces.
    if let Err(TransportError::ErrorResp(ref e)) = provider
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
        let tx = match provider
            .get_transaction_by_sender_nonce(operator_address, nonce)
            .await
        {
            Err(err) => {
                anyhow::bail!("Failed to fetch in-flight transaction at nonce {nonce}: {err}");
            }
            Ok(Some(tx)) => tx,
            Ok(None) => {
                // The tx was dropped from the mempool. Providers that support
                // `eth_getTransactionBySenderAndNonce` return the tx whether it is pending or
                // already mined, so `None` unambiguously means it no longer exists.
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
                cmd.solidity_call(gateway, &operator_address) == *tx.input()
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
                let Some(L1SenderCommand::SendToL1(cmd)) = inbound.recv().await else {
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
    // Use the minimum of estimated and configured values for gas fields
    let capped_max_fee_per_gas = if eip1559_est.max_fee_per_gas > max_fee_per_gas {
        tracing::warn!(
            "L1 sender's configured maxFeePerGas ({max_fee_per_gas}) \
             is lower than the one estimated from network  ({}), \
             using the configured base fee value ({max_fee_per_gas}) - this may result in inclusion delay.",
            eip1559_est.max_fee_per_gas
        );
        max_fee_per_gas
    } else {
        eip1559_est.max_fee_per_gas
    };
    let capped_max_priority_fee_per_gas = if eip1559_est.max_priority_fee_per_gas
        > max_priority_fee_per_gas
    {
        tracing::warn!(
            "L1 sender's configured max_priority_fee_per_gas ({max_priority_fee_per_gas}) \
             is lower than the one estimated from network  ({}), \
             using the configured priority fee value ({max_priority_fee_per_gas}) - this may result in inclusion delay.",
            eip1559_est.max_priority_fee_per_gas
        );
        max_priority_fee_per_gas
    } else {
        eip1559_est.max_priority_fee_per_gas
    };

    let tx = TransactionRequest::default()
        .with_from(operator_address)
        .with_max_fee_per_gas(capped_max_fee_per_gas)
        .with_max_priority_fee_per_gas(capped_max_priority_fee_per_gas)
        .with_gas_limit(MAX_TX_GAS_USED);
    Ok(tx)
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
