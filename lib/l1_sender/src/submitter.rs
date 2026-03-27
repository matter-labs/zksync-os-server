use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::error::{L1SendError, RecoverableReason};
use crate::{
    ExponentialBackoff, InFlightTx, TRANSACTION_TIMEOUT, reason_label, report_balance_and_nonce,
};
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use alloy::consensus::BlobTransactionValidationError;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::Address;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{Provider, WalletProvider};
use alloy::rpc::types::TransactionRequest;
use futures::FutureExt;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateHandle;
use zksync_os_pipeline::PeekableReceiver;

// ==============================================================================
// Submitter
// ==============================================================================

/// Reads commands from the upstream channel and the resubmit channel, estimates
/// L1 fees, builds and submits transactions, and forwards `InFlightTx` items to
/// the `Watcher`.
///
/// The Submitter is the only task that holds a reference to the L1 provider.
/// All resubmission (after timeout or transient receipt error) comes back through
/// the `resubmit_rx` channel, which is checked before blocking on the upstream.
pub(crate) struct Submitter<Input, F, P>
where
    Input: SendToL1,
    F: TxFiller<Ethereum>,
    P: Provider<Ethereum>,
{
    pub(crate) inbound: PeekableReceiver<L1SenderCommand<Input>>,
    pub(crate) resubmit_rx: mpsc::Receiver<Input>,
    pub(crate) in_flight_tx: mpsc::Sender<InFlightTx<Input>>,
    pub(crate) provider: FillProvider<F, P>,
    pub(crate) config: L1SenderConfig<Input>,
    pub(crate) to_address: Address,
    pub(crate) operator_address: Address,
    pub(crate) gateway: bool,
    pub(crate) pending_commands: Vec<Input>,
    pub(crate) latency_tracker: ComponentStateHandle<L1SenderState>,
    pub(crate) backoff: ExponentialBackoff,
    pub(crate) cmd_buffer: Vec<L1SenderCommand<Input>>,
}

impl<Input, F, P> Submitter<Input, F, P>
where
    Input: SendToL1,
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum> + Clone + 'static,
{
    // ==============================================================================
    // Main loop
    // ==============================================================================

    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            // Wait for commands if there is nothing pending.
            // Resubmit commands (from Watcher) are prioritised over new upstream commands.
            if self.pending_commands.is_empty() {
                match self.receive().await {
                    Ok(()) => {}
                    Err(e) => return Err(e.into_anyhow()),
                }
            }

            match self.send_pending().await {
                Ok(()) => {
                    report_balance_and_nonce::<_, Input>(&self.provider, self.operator_address)
                        .await;
                    self.backoff.reset();
                }
                Err(L1SendError::Fatal(e)) => return Err(e),
                Err(L1SendError::Transient(e)) => {
                    self.handle_transient(e, "during send").await;
                }
                Err(L1SendError::Recoverable { reason, source }) => {
                    self.handle_recoverable(reason, source, "during send").await;
                }
            }
        }
    }

    // ==============================================================================
    // Receive
    // ==============================================================================

    /// Fills `pending_commands` from either the resubmit channel (priority) or
    /// the upstream inbound channel. Blocks until at least one command is available.
    async fn receive(&mut self) -> Result<(), L1SendError> {
        self.latency_tracker.enter_state(L1SenderState::WaitingRecv);

        // Drain any already-queued resubmit commands without blocking.
        let mut got_resubmit = false;
        while let Ok(cmd) = self.resubmit_rx.try_recv() {
            self.pending_commands.push(cmd);
            got_resubmit = true;
        }
        if got_resubmit {
            return Ok(());
        }

        // Block on whichever channel fires first, preferring resubmit.
        tokio::select! {
            biased;
            Some(cmd) = self.resubmit_rx.recv() => {
                self.pending_commands.push(cmd);
                // Drain any additional resubmits that arrived concurrently.
                while let Ok(cmd) = self.resubmit_rx.try_recv() {
                    self.pending_commands.push(cmd);
                }
            }
            received = self.inbound.recv_many(&mut self.cmd_buffer, self.config.command_limit) => {
                if received == 0 {
                    return Err(L1SendError::Fatal(anyhow::anyhow!("inbound channel closed")));
                }
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
            }
        }
        Ok(())
    }

    // ==============================================================================
    // Send pending
    // ==============================================================================

    /// Submits each pending command as an L1 transaction, sending each
    /// `InFlightTx` to the Watcher on success. Partial progress is preserved:
    /// commands that have been sent move out of `pending_commands` one at a time.
    async fn send_pending(&mut self) -> Result<(), L1SendError> {
        self.latency_tracker.enter_state(L1SenderState::SendingToL1);

        // Estimate EIP-1559 fees once for the whole batch.
        let eip1559_est = self
            .provider
            .estimate_eip1559_fees()
            .await
            .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?;

        L1_SENDER_METRICS.report_l1_eip_1559_estimation(eip1559_est);

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

        let max_fee_per_gas = eip1559_est.max_fee_per_gas;
        let max_priority_fee_per_gas = eip1559_est
            .max_priority_fee_per_gas
            .min(self.config.max_priority_fee_per_gas_wei);

        while !self.pending_commands.is_empty() {
            let cmd = &self.pending_commands[0];

            let mut tx_request = TransactionRequest::default()
                .with_from(self.operator_address)
                .with_to(self.to_address)
                .with_input(cmd.solidity_call(self.gateway, &self.operator_address))
                .with_max_fee_per_gas(max_fee_per_gas)
                .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
                .with_gas_limit(15_000_000);

            if let Some(blob_sidecar) = cmd.blob_sidecar() {
                let fee_per_blob_gas = self
                    .provider
                    .get_blob_base_fee()
                    .await
                    .map_err(|e| L1SendError::Transient(anyhow::Error::from(e)))?;

                L1_SENDER_METRICS.report_blob_base_fee(fee_per_blob_gas);

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

            // Command successfully submitted — move it to in_flight via the Watcher channel.
            let mut cmd = self.pending_commands.remove(0);
            cmd.as_mut()
                .iter_mut()
                .for_each(|envelope| envelope.set_stage(Input::SENT_STAGE));

            self.in_flight_tx
                .send(InFlightTx { command: cmd, tx_hash, receipt_future })
                .await
                .map_err(|_| {
                    L1SendError::Fatal(anyhow::anyhow!("in_flight channel closed (Watcher died)"))
                })?;
        }

        Ok(())
    }

    // ==============================================================================
    // Error helpers
    // ==============================================================================

    async fn handle_transient(&mut self, e: anyhow::Error, context: &str) {
        tracing::warn!(
            ?e,
            command_name = Input::NAME,
            "transient error {context}, entering backoff"
        );
        L1_SENDER_METRICS.transient_errors.inc();
        self.latency_tracker.enter_state(L1SenderState::TransientBackoff);
        let delay = self.backoff.next();
        tokio::time::sleep(delay).await;
    }

    async fn handle_recoverable(
        &mut self,
        reason: RecoverableReason,
        source: anyhow::Error,
        context: &str,
    ) {
        let state = match reason {
            RecoverableReason::GasBlocked => L1SenderState::GasBlocked,
            RecoverableReason::BlobFeeBlocked => L1SenderState::BlobFeeBlocked,
            _ => L1SenderState::TransientBackoff,
        };
        tracing::warn!(
            ?source,
            ?reason,
            command_name = Input::NAME,
            "recoverable error {context}"
        );
        L1_SENDER_METRICS.recoverable_errors[&reason_label(reason)].inc();
        self.latency_tracker.enter_state(state);
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
