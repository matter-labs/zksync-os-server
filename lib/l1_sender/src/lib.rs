pub mod batcher_metrics;
pub mod batcher_model;
pub mod commands;
pub mod config;
mod metrics;
pub mod pipeline_component;
pub mod types;
pub mod upgrade_gatekeeper;

use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use crate::types::GasParams;
use alloy::consensus::BlobTransactionValidationError;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::Address;
use alloy::primitives::TxHash;
use alloy::primitives::utils::{format_ether, format_units};
use alloy::providers::ext::DebugApi;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{PendingTransactionBuilder, Provider, WalletProvider, WatchTxError};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use anyhow::Context as _;
use tokio::sync::mpsc::Sender;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zksync_os_observability::{ComponentStateHandle, ComponentStateReporter};
use zksync_os_operator_signer::SignerConfig;
use zksync_os_pipeline::PeekableReceiver;

/// Process responsible for sending transactions to L1.
/// Handles one type of l1 command (e.g. Commit or Prove).
/// Loads up to `command_limit` commands from the channel and sends them to L1 in parallel.
/// Each command is spawned as an independent Tokio task via [`submit_and_confirm`], which
/// handles submission, receipt polling, and resubmission with gas bumps.
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
pub async fn run_l1_sender<Input, F, P>(
    // == plumbing ==
    mut inbound: PeekableReceiver<L1SenderCommand<Input>>,
    outbound: Sender<SignedBatchEnvelope<FriProof>>,

    // == command-specific settings ==
    to_address: Address,

    // == config ==
    mut provider: FillProvider<F, P>,
    config: L1SenderConfig<Input>,
    gateway: bool,
    commit_submitted_tx: Option<watch::Sender<u64>>,
) -> anyhow::Result<()>
where
    Input: SendToL1 + Send + Sync + 'static,
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet> + Clone + Send + 'static,
    P: Provider<Ethereum> + Clone + Send + Sync + 'static,
{
    let latency_tracker =
        ComponentStateReporter::global().handle_for(Input::NAME, L1SenderState::WaitingRecv);
    let command_name = Input::NAME;

    let (operator_address, mut next_nonce) =
        register_operator::<_, Input>(&mut provider, config.operator_signer.clone()).await?;

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
        // This method only returns `0` if the channel has been closed and there are no more items
        // in the queue.
        if received == 0 {
            tracing::info!("inbound channel closed");
            return Ok(());
        }

        // Collect task handles in submission order so we can await them in that
        // same order below, guaranteeing `outbound` sees envelopes in nonce order.
        let mut handles: Vec<JoinHandle<anyhow::Result<Vec<SignedBatchEnvelope<FriProof>>>>> =
            Vec::with_capacity(cmd_buffer.len());

        latency_tracker.enter_state(L1SenderState::SendingToL1);
        for cmd in cmd_buffer.drain(..) {
            match cmd {
                L1SenderCommand::Passthrough(batch) => anyhow::bail!(
                    "Unexpected passthrough command for batch {:?}. \
                    No passthrough commands are expected after the first `SendToL1`.",
                    batch.batch_number()
                ),

                L1SenderCommand::SendToL1(mut command) => {
                    // Estimate current network fees and clamp to the configured caps.
                    // Even when the network estimate exceeds the cap we still submit — the
                    // nonce slot must be filled to keep the pipeline moving — but we never
                    // broadcast above the operator's stated maximum.
                    let caps = GasParams {
                        max_fee_per_gas: config.max_fee_per_gas_wei,
                        max_priority_fee_per_gas: config.max_priority_fee_per_gas_wei,
                        fee_per_blob_gas: config.max_fee_per_blob_gas_wei,
                    };
                    let raw = estimate_gas_params(&provider, &caps).await;
                    let gas_params = raw.clamped_to(&caps);
                    if let Err(err) = L1_SENDER_METRICS.report_fee_caps(command_name, &caps) {
                        tracing::warn!(%err, command_name, "failed to report fee cap metrics");
                    }
                    if raw.exceeds(&caps) {
                        tracing::warn!(
                            command_name,
                            configured_max_fee = caps.max_fee_per_gas,
                            estimated_max_fee = raw.max_fee_per_gas,
                            configured_max_priority_fee = caps.max_priority_fee_per_gas,
                            estimated_max_priority_fee = raw.max_priority_fee_per_gas,
                            configured_max_blob_fee = caps.fee_per_blob_gas,
                            estimated_blob_fee = raw.fee_per_blob_gas,
                            "network fees exceed configured cap — submitting at cap, inclusion might be delayed if the cap is too low",
                        );
                    }

                    let nonce = next_nonce;
                    next_nonce += 1;

                    // Broadcast the first transaction sequentially before moving on to
                    // the next nonce.  If this fails, no higher-nonce transactions will
                    // have been broadcast yet, so there are no orphaned mempool entries.
                    let (tx_hash, gas_params) = build_and_send(
                        &command,
                        &gas_params,
                        nonce,
                        &provider,
                        &config,
                        to_address,
                        operator_address,
                        gateway,
                    )
                    .await?;

                    // Notify CommitWatcher: this batch number has been submitted to L1.
                    if let Some(sender) = &commit_submitted_tx {
                        let batch_number = command
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

                    // Mark envelopes as sent now that the tx is in the mempool.
                    command
                        .as_mut()
                        .iter_mut()
                        .for_each(|e| e.set_stage(Input::SENT_STAGE));

                    // Spawn the receipt-watching / resubmission loop in parallel so
                    // multiple batches can be confirmed concurrently once all first
                    // transactions have been broadcast.
                    let provider = provider.clone();
                    let config = config.clone();
                    let handle = tokio::spawn(async move {
                        watch_and_resubmit(
                            command,
                            nonce,
                            tx_hash,
                            gas_params,
                            provider,
                            config,
                            to_address,
                            operator_address,
                            gateway,
                        )
                        .await
                    });
                    handles.push(handle);
                }
            }
        }

        // Await in submission order to preserve nonce ordering on `outbound`.
        latency_tracker.enter_state(L1SenderState::WaitingL1Inclusion);
        L1_SENDER_METRICS.parallel_transactions[&command_name].set(handles.len() as u64);

        for handle in handles {
            let envelopes = handle.await.context("watch_and_resubmit task panicked")??;
            latency_tracker.enter_state(L1SenderState::WaitingSend);
            for envelope in envelopes {
                outbound
                    .send(envelope)
                    .await
                    .context("outbound channel closed")?;
            }
            latency_tracker.enter_state(L1SenderState::WaitingL1Inclusion);
        }
    }
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

/// Watches a submitted L1 transaction for confirmation, resubmitting with bumped fees
/// on each timeout until a receipt is received.
///
/// The caller is responsible for having already broadcast the first transaction
/// (sequentially, before spawning this task) so that a submission failure for nonce N
/// does not leave higher-nonce transactions stranded in the mempool.
///
/// Returns the confirmed envelopes with the `MINED_STAGE` set.
#[allow(clippy::too_many_arguments)]
async fn watch_and_resubmit<Input, F, P>(
    command: Input,
    nonce: u64,
    mut tx_hash: TxHash,
    mut gas_params: GasParams,
    provider: FillProvider<F, P>,
    config: L1SenderConfig<Input>,
    to_address: Address,
    operator_address: Address,
    gateway: bool,
) -> anyhow::Result<Vec<SignedBatchEnvelope<FriProof>>>
where
    Input: SendToL1,
    F: TxFiller<Ethereum>,
    P: Provider<Ethereum> + Clone,
{
    let command_name = Input::NAME;

    loop {
        let watch_result = PendingTransactionBuilder::new(provider.root().clone(), tx_hash)
            .with_timeout(Some(config.transaction_timeout))
            .get_receipt()
            .await;

        match watch_result {
            Ok(receipt) => {
                validate_tx_receipt(&provider, &command, receipt).await?;
                if let Err(err) =
                    try_report_post_confirmation(command_name, operator_address, &provider).await
                {
                    tracing::warn!(%err, command_name, "failed to report post-confirmation metrics");
                }
                let mut envelopes: Vec<SignedBatchEnvelope<FriProof>> = command.into();
                envelopes
                    .iter_mut()
                    .for_each(|e| e.set_stage(Input::MINED_STAGE));
                return Ok(envelopes);
            }

            Err(alloy::providers::PendingTransactionError::TxWatcher(WatchTxError::Timeout)) => {
                tracing::warn!(
                    command_name,
                    tx_hash = ?tx_hash,
                    nonce,
                    "transaction timed out — evaluating resubmission",
                );

                // Raw fee estimate — no cap blocking.  The nonce slot is already reserved;
                // blocking here would leave the pipeline stuck with an unconfirmed tx.
                let fresh = estimate_gas_params(
                    &provider,
                    &GasParams {
                        max_fee_per_gas: config.max_fee_per_gas_wei,
                        max_priority_fee_per_gas: config.max_priority_fee_per_gas_wei,
                        fee_per_blob_gas: config.max_fee_per_blob_gas_wei,
                    },
                )
                .await;

                // Always apply at least a 10% bump to guarantee mempool acceptance.
                // If the bumped fees exceed the cap, rewatch the original instead.
                let bumped = fresh.with_minimum_replacement_bump(&gas_params);
                if bumped.max_fee_per_gas <= config.max_fee_per_gas_wei
                    && bumped.max_priority_fee_per_gas <= config.max_priority_fee_per_gas_wei
                {
                    tracing::info!(
                        command_name,
                        nonce,
                        "sending replacement tx with bumped fees",
                    );
                    L1_SENDER_METRICS.tx_resubmissions[&command_name].inc();
                    let (new_hash, new_gas) = build_and_send(
                        &command,
                        &bumped,
                        nonce,
                        &provider,
                        &config,
                        to_address,
                        operator_address,
                        gateway,
                    )
                    .await?;
                    tx_hash = new_hash;
                    gas_params = new_gas;
                } else {
                    tracing::info!(
                        command_name,
                        tx_hash = ?tx_hash,
                        "bumped fees would exceed cap — re-watching original tx",
                    );
                }
            }

            Err(e) => {
                return Err(anyhow::anyhow!(e).context("wait for L1 transaction confirmation"));
            }
        }
    }
}

/// Estimates current network fees from the provider, falling back to `fallback` per-field
/// if an RPC call fails (e.g. provider is temporarily unavailable).  Using the configured
/// caps as fallback means we still submit — potentially above the network price — rather
/// than stalling the pipeline on a transient RPC error.
async fn estimate_gas_params(provider: &impl Provider, fallback: &GasParams) -> GasParams {
    let (max_fee_per_gas, max_priority_fee_per_gas) = match provider.estimate_eip1559_fees().await {
        Ok(est) => {
            if let Err(err) = L1_SENDER_METRICS.report_l1_eip_1559_estimation(est) {
                tracing::warn!(%err, "failed to report EIP-1559 estimation metrics");
            }
            (est.max_fee_per_gas, est.max_priority_fee_per_gas)
        }
        Err(err) => {
            tracing::warn!(
                %err,
                fallback_max_fee_per_gas = fallback.max_fee_per_gas,
                fallback_max_priority_fee_per_gas = fallback.max_priority_fee_per_gas,
                "failed to estimate EIP-1559 fees — using configured caps as fallback",
            );
            (fallback.max_fee_per_gas, fallback.max_priority_fee_per_gas)
        }
    };

    let fee_per_blob_gas = match provider.get_blob_base_fee().await {
        Ok(fee) => {
            if let Err(err) = L1_SENDER_METRICS.report_blob_base_fee(fee) {
                tracing::warn!(%err, "failed to report blob base fee metric");
            }
            fee
        }
        Err(err) => {
            tracing::warn!(
                %err,
                fallback_fee_per_blob_gas = fallback.fee_per_blob_gas,
                "failed to get blob base fee — using configured cap as fallback",
            );
            fallback.fee_per_blob_gas
        }
    };

    tracing::debug!(
        max_fee_per_gas_gwei = ?format_units(max_fee_per_gas, "gwei"),
        max_priority_fee_per_gas_gwei = ?format_units(max_priority_fee_per_gas, "gwei"),
        fee_per_blob_gas_gwei = ?format_units(fee_per_blob_gas, "gwei"),
        "estimated fees",
    );

    GasParams {
        max_fee_per_gas,
        max_priority_fee_per_gas,
        fee_per_blob_gas,
    }
}

/// Builds an L1 transaction from `command` and broadcasts it.
///
/// Returns `(tx_hash, gas_params_used)` on success.  The returned `GasParams` are the
/// values actually embedded in the signed transaction — callers use them for the
/// resubmission bump check.
#[allow(clippy::too_many_arguments)]
async fn build_and_send<Input, F, P>(
    command: &Input,
    gas_params: &GasParams,
    nonce: u64,
    provider: &FillProvider<F, P>,
    config: &L1SenderConfig<Input>,
    to_address: Address,
    operator_address: Address,
    gateway: bool,
) -> anyhow::Result<(TxHash, GasParams)>
where
    Input: SendToL1,
    F: TxFiller<Ethereum>,
    P: Provider<Ethereum>,
{
    // Build the base EIP-1559 request.  The nonce is set explicitly so the provider's
    // automatic nonce filler does not interfere with our manual nonce tracking.
    let mut tx_request = TransactionRequest::default()
        .with_from(operator_address)
        .with_to(to_address)
        .with_nonce(nonce)
        .with_max_fee_per_gas(gas_params.max_fee_per_gas)
        .with_max_priority_fee_per_gas(gas_params.max_priority_fee_per_gas)
        // Default value for `max_aggregated_tx_gas` from zksync-era — should always be enough.
        .with_gas_limit(15_000_000)
        .with_input(command.solidity_call(gateway, &operator_address));

    if let Some(blob_sidecar) = command.blob_sidecar() {
        tx_request.set_max_fee_per_blob_gas(gas_params.fee_per_blob_gas);
        tx_request.set_blob_sidecar(blob_sidecar);
    }

    // Fill remaining fields (chain-id, access-list, …) and sign the transaction.
    let envelope = provider
        .fill(tx_request)
        .await
        .context("fill transaction")?
        .try_into_envelope()
        .context("convert to typed envelope")?
        .try_into_pooled()
        .context("convert to pooled envelope")?;

    // If the Fusaka upgrade has activated, convert the EIP-4844 blob sidecar to EIP-7594.
    // TODO: make this conversion unconditional once anvil supports EIP-7594 blobs
    //       (see https://github.com/foundry-rs/foundry/issues/12222).
    let pending_block = provider
        .get_block(BlockId::pending())
        .await
        .context("get pending block")?
        .expect("pending block must always be present");

    let tx = if config.fusaka_upgrade_timestamp <= pending_block.header.timestamp {
        envelope
            .try_map_eip4844(|tx| {
                tx.try_map_sidecar(|sidecar| {
                    Ok::<_, BlobTransactionValidationError>(BlobTransactionSidecarVariant::Eip7594(
                        sidecar.try_into_eip7594()?,
                    ))
                })
            })
            .context("convert blob sidecar to EIP-7594")?
    } else {
        envelope
    };

    let tx_hash = provider
        .send_raw_transaction(&tx.encoded_2718())
        .await
        .context("send raw transaction")?
        .tx_hash()
        .to_owned();

    tracing::info!(
        command_name = Input::NAME,
        tx_hash = ?tx_hash,
        nonce,
        max_fee_per_gas = gas_params.max_fee_per_gas,
        max_priority_fee_per_gas = gas_params.max_priority_fee_per_gas,
        "L1 transaction submitted",
    );

    Ok((tx_hash, gas_params.clone()))
}

async fn try_report_post_confirmation(
    command_name: &'static str,
    operator_address: Address,
    provider: &impl Provider<Ethereum>,
) -> anyhow::Result<()> {
    let balance = provider
        .get_balance(operator_address)
        .await
        .context("get operator balance")?;
    let nonce = provider
        .get_transaction_count(operator_address)
        .await
        .context("get operator nonce")?;
    let balance_eth = format_ether(balance);
    tracing::info!(
        command_name,
        balance_eth,
        nonce,
        "post-confirmation operator state"
    );
    L1_SENDER_METRICS.balance[&command_name].set(balance_eth.parse()?);
    L1_SENDER_METRICS.nonce[&command_name].set(nonce);
    Ok(())
}

// ==============================================================================
// Operator registration
// ==============================================================================

/// Registers the operator signer with the provider wallet and returns `(address, nonce)`.
///
/// The nonce is fetched once at startup so the main loop can track it manually.  We
/// set `.with_nonce` on every outgoing transaction to bypass alloy's automatic nonce
/// filler, which would otherwise interfere with our manual counter.
async fn register_operator<
    P: Provider<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    Input: SendToL1,
>(
    provider: &mut P,
    signer_config: SignerConfig,
) -> anyhow::Result<(Address, u64)> {
    let address = signer_config
        .register_with_wallet(provider.wallet_mut())
        .await
        .context("register operator with wallet")?;

    let balance = provider
        .get_balance(address)
        .await
        .context("get operator balance")?;
    let nonce = provider
        .get_transaction_count(address)
        .await
        .context("get operator nonce")?;

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
        nonce,
        "initialized L1 sender",
    );
    Ok((address, nonce))
}

// ==============================================================================
// Receipt validation
// ==============================================================================

/// Validates a confirmed transaction receipt.
///
/// Successful receipts: records gas/cost metrics and returns `Ok(())`.
/// Reverted transactions: logs the error with an optional debug trace and returns `Err`.
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
