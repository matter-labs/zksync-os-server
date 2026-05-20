pub mod commands;
pub mod config;
mod metrics;
pub mod pipeline_component;
pub mod upgrade_gatekeeper;

use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderFeeConfig;
use crate::metrics::{L1_SENDER_METRICS, PriorityFeeEstimatePercentile, PriorityFeeEstimateWindow};
use crate::pipeline_component::L1Sender;
use alloy::consensus::BlobTransactionValidationError;
use alloy::consensus::Transaction as ConsensusTransaction;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, BlockNumberOrTag, Encodable2718};
use alloy::network::{
    Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844, TransactionResponse,
};
use alloy::primitives::utils::{format_ether, format_units};
use alloy::primitives::{Address, B256};
use alloy::providers::ext::DebugApi;
use alloy::providers::fillers::TxFiller;
use alloy::providers::utils::Eip1559Estimation;
use alloy::providers::{Provider, WalletProvider};
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::transports::TransportError;
use anyhow::Context as _;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use zksync_os_batch_types::batcher_model::{FriProof, SignedBatchEnvelope};
use zksync_os_observability::{ComponentStateReporter, GenericComponentState, StateLabel};
use zksync_os_pipeline::{PeekableReceiver, SendAndRecordExt};

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

const REQUIRED_CONFIRMATIONS_L1: u64 = 3;
/// In case there's only one chain connected to gateway, it is very likely that there will be not enough block production
/// to reach 3 confirmations for such transactions
const REQUIRED_CONFIRMATIONS_GATEWAY: u64 = 1;
const OPERATOR_METRICS_POLL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct FeeParams {
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    max_fee_per_blob_gas: u128,
}

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
impl<LF, LP, Input> L1Sender<LF, LP, Input>
where
    LF: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet> + 'static,
    LP: Provider<Ethereum> + Clone + 'static,
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
        let command_name = Input::COMPONENT_ID.as_str();
        let fee_config = self.config.fee_config;
        let force_transaction_resubmission = self.config.force_transaction_resubmission;

        let mut cmd_buffer = Vec::with_capacity(self.config.command_limit);
        // Process all potential passthrough commands first
        if self
            .process_prepending_passthrough_commands(&mut inbound, &outbound, &state_reporter)
            .await?
            .is_none()
        {
            tracing::info!("inbound channel closed");
            return Ok(());
        }

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
            state_reporter.enter_state(L1SenderState::SendingToL1);
            let range = Input::display_range(&commands); // Only for logging
            tracing::info!(command_name, range, "sending L1 transactions");
            L1_SENDER_METRICS.parallel_transactions[&command_name].set(commands.len() as u64);
            // It's important to preserve the order of commands -
            // so that we send them downstream also in order.
            // This holds true because l1 transactions are included in the order of sender nonce.
            // Keep this in mind if changing sending logic (that is, if adding `buffer` we'd need to set nonce manually)
            let pending_txs: Vec<PendingTx<Input>> =
            futures::stream::iter(commands.drain(..))
                .then(|mut cmd| async {
                    let fee_params = self
                        .resolve_fee_params(
                        fee_config,
                        force_transaction_resubmission,
                    )
                    .await?;
                    let operator_address = self.operator_address().await?;
                    let mut tx_request = TransactionRequest::default()
                        .with_from(operator_address)
                        .with_max_fee_per_gas(fee_params.max_fee_per_gas)
                        .with_max_priority_fee_per_gas(fee_params.max_priority_fee_per_gas)
                        .with_gas_limit(15000000)
                        .with_to(self.to_address)
                        .with_input(cmd.solidity_call(self.gateway, &operator_address));

                    if let Some(blob_sidecar) = cmd.blob_sidecar() {
                        let fee_per_blob_gas = self.provider.get_blob_base_fee().await?;
                        L1_SENDER_METRICS
                            .report_blob_base_fee(fee_per_blob_gas)?;
                        let max_fee_per_blob_gas = fee_params.max_fee_per_blob_gas;

                        if fee_per_blob_gas > max_fee_per_blob_gas {
                            tracing::warn!(
                                max_fee_per_blob_gas,
                                fee_per_blob_gas,
                                "L1 sender's configured maxFeePerBlobGas is lower than the one estimated from network"
                            );
                        }
                        tx_request.set_max_fee_per_blob_gas(max_fee_per_blob_gas);
                        tx_request
                            .set_blob_sidecar(BlobTransactionSidecarVariant::Eip4844(blob_sidecar));
                    };

                    // Fill the transaction (e.g., nonce, gas, etc.) using the provider and convert it to an
                    // envelope.
                    let envelope = self.provider.fill(tx_request).await?.try_into_envelope()?.try_into_pooled()?;

                    let pending_block = self.provider.get_block(BlockId::pending()).await?.expect("no pending block");
                    // todo: make conversion unconditional (and remove respective config) once anvil
                    //       supports EIP-7594 blobs (see https://github.com/foundry-rs/foundry/issues/12222)
                    let tx = if self.config.fusaka_upgrade_timestamp <= pending_block.header.timestamp {
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

                    let pending_tx = self.provider
                        .send_raw_transaction(&tx.encoded_2718())
                        .await?;
                    let submitted_at = Instant::now();
                    let tx_hash = *pending_tx.tx_hash();
                    let receipt_fut = self.wait_for_confirmed_receipt(tx_hash);
                    tracing::info!(
                        "{command_name}: L1 transaction submitted for {range}. Hash: {tx_hash:?} Waiting for inclusion...",
                    );

                    // Notify CommitWatcher: this batch number has been submitted to L1.
                    if let Some(sender) = &self.commit_submitted_tx {
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
            self.wait_for_txs_and_forward(pending_txs, &state_reporter, &outbound)
                .await?;
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

    fn wait_for_confirmed_receipt(&self, tx_hash: B256) -> TransactionReceiptFuture {
        let provider = self.provider.root().clone();
        let required_confirmations = if self.gateway {
            REQUIRED_CONFIRMATIONS_GATEWAY
        } else {
            REQUIRED_CONFIRMATIONS_L1
        };
        let timeout = self.config.transaction_timeout;
        async move {
            let started_at = Instant::now();
            let poll_interval = provider.client().poll_interval();
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
    /// `sl_block_number` must be the same L1 block at which `getTotalBatches*` was called when
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
            .block_id(BlockId::number(self.sl_block_number))
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
            sl_block_number = self.sl_block_number,
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
                    cmd.solidity_call(self.gateway, &operator_address) == *tx.input()
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

    async fn resolve_fee_params(
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
        self.report_custom_priority_fee_metrics().await?;

        tracing::debug!(
            max_priority_fee_per_gas_gwei = ?format_units(estimated.max_priority_fee_per_gas, "gwei"),
            max_fee_per_gas_gwei = ?format_units(estimated.max_fee_per_gas, "gwei"),
            "estimated priority and max fees"
        );

        Ok(apply_fee_caps(configured_params, estimated))
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

    async fn validate_tx_receipt(
        &self,
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
