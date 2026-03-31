use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use crate::error::is_transient;
use crate::metrics::{L1SenderState, L1_SENDER_METRICS};
use crate::types::{Backoff, InFlightItem, InFlightTx, ResubmitRequest};
use crate::validate_tx_receipt;
use alloy::network::Ethereum;
use alloy::primitives::utils::format_ether;
use alloy::providers::Provider;
use anyhow::Context;
use std::time::Duration;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateHandle;

// ==============================================================================
// Watcher
// ==============================================================================

/// Responsible for waiting for submitted transactions to be mined and
/// forwarding confirmed commands downstream.
///
/// Processes in-flight items sequentially (one at a time) to preserve output
/// ordering — the `in_flight` channel is FIFO, so items arrive in submission
/// order, and the Watcher emits them to `outbound` in the same order.
///
/// On a 300 s timeout: sends a `ResubmitRequest` to the Submitter and waits
/// for a replacement item from `in_flight` before continuing.
pub(crate) struct Watcher<P, Input>
where
    P: Provider<Ethereum>,
    Input: SendToL1,
{
    pub in_flight_rx: mpsc::Receiver<InFlightItem<Input>>,
    pub resubmit_tx: mpsc::Sender<ResubmitRequest<Input>>,
    pub outbound: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
    pub provider: P,
    pub operator_address: alloy::primitives::Address,
    pub poll_interval: Duration,
    pub transaction_timeout: Duration,
    pub latency_tracker: ComponentStateHandle<L1SenderState>,
}

impl<P, Input> Watcher<P, Input>
where
    P: Provider<Ethereum>,
    Input: SendToL1 + Send + 'static,
{
    pub async fn run(mut self) -> anyhow::Result<()> {
        while let Some(item) = self.in_flight_rx.recv().await {
            self.process_item(item).await?;
        }
        tracing::info!(command_name = Input::NAME, "in_flight channel closed");
        Ok(())
    }

    async fn process_item(&mut self, item: InFlightItem<Input>) -> anyhow::Result<()> {
        match item {
            InFlightItem::Passthrough(envelope) => {
                self.outbound
                    .send(*envelope)
                    .await
                    .map_err(|_| anyhow::anyhow!("outbound channel closed"))?;
            }
            InFlightItem::Tx(in_flight) => {
                self.latency_tracker
                    .enter_state(L1SenderState::WaitingL1Inclusion);
                self.watch_until_confirmed(in_flight).await?;
            }
        }
        Ok(())
    }

    // ==============================================================================
    // Transaction Confirmation Loop
    // ==============================================================================

    /// Poll for a receipt, handling timeout by triggering resubmission.
    ///
    /// On timeout: sends `ResubmitRequest` to Submitter, then reads the
    /// replacement item from `in_flight` and watches that instead —
    /// looping until the tx is eventually confirmed.
    async fn watch_until_confirmed(
        &mut self,
        mut in_flight: InFlightTx<Input>,
    ) -> anyhow::Result<()> {
        loop {
            match self.poll_for_receipt(in_flight.tx_hash).await? {
                PollOutcome::Receipt(receipt) => {
                    validate_tx_receipt(&self.provider, &in_flight.command, receipt).await?;
                    self.report_post_confirmation(Input::NAME).await;
                    self.latency_tracker.enter_state(L1SenderState::WaitingSend);
                    for mut envelope in in_flight.command.into() {
                        envelope.set_stage(Input::MINED_STAGE);
                        self.outbound
                            .send(envelope)
                            .await
                            .map_err(|_| anyhow::anyhow!("outbound channel closed"))?;
                    }
                    return Ok(());
                }
                PollOutcome::TimedOut => {
                    let InFlightTx {
                        tx_hash: orig_hash,
                        gas_params: orig_gas,
                        command,
                        nonce,
                    } = in_flight;
                    tracing::warn!(
                        command_name = Input::NAME,
                        tx_hash = ?orig_hash,
                        nonce,
                        "transaction timed out, requesting resubmission",
                    );
                    self.resubmit_tx
                        .send(ResubmitRequest {
                            original_tx_hash: orig_hash,
                            original_gas_params: orig_gas,
                            command,
                            nonce,
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("resubmit channel closed"))?;

                    // Block until the Submitter puts a replacement item on in_flight.
                    let replacement = self
                        .in_flight_rx
                        .recv()
                        .await
                        .ok_or_else(|| {
                            anyhow::anyhow!("in_flight channel closed during resubmit wait")
                        })?;

                    match replacement {
                        InFlightItem::Tx(new_in_flight) => {
                            in_flight = new_in_flight;
                            // Continue the outer loop to watch the replacement.
                        }
                        InFlightItem::Passthrough(_) => {
                            anyhow::bail!("unexpected passthrough received while waiting for resubmission replacement");
                        }
                    }
                }
            }
        }
    }

    // ==============================================================================
    // Receipt Polling
    // ==============================================================================

    /// Poll `get_transaction_receipt` with backoff until a receipt arrives or
    /// `transaction_timeout` elapses.
    async fn poll_for_receipt(
        &self,
        tx_hash: alloy::primitives::TxHash,
    ) -> anyhow::Result<PollOutcome> {
        let deadline = tokio::time::Instant::now() + self.transaction_timeout;
        let mut backoff = Backoff::new();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(PollOutcome::TimedOut);
            }

            // Convert to anyhow::Error up front so that `is_transient` can
            // downcast to `RpcError<TransportErrorKind>` in both branches.
            match self
                .provider
                .get_transaction_receipt(tx_hash)
                .await
                .map_err(anyhow::Error::new)
            {
                Ok(Some(receipt)) => return Ok(PollOutcome::Receipt(receipt)),
                Ok(None) => {
                    // Transaction not yet mined — sleep for the shorter of the
                    // poll interval and the remaining deadline budget.
                    backoff.reset();
                    tokio::time::sleep(self.poll_interval.min(remaining)).await;
                }
                Err(e) if is_transient(&e) => {
                    tracing::warn!(
                        delay = ?backoff.current(),
                        "transient error polling receipt, retrying"
                    );
                    backoff.wait().await;
                    backoff.advance();
                }
                Err(e) => {
                    return Err(e).context("get_transaction_receipt");
                }
            }
        }
    }

    // ==============================================================================
    // Post-Confirmation Metrics
    // ==============================================================================

    /// Report balance and nonce to Prometheus after a successful confirmation.
    /// Failures are non-fatal — metrics gaps are preferable to crashing the pipeline.
    async fn report_post_confirmation(&self, command_name: &'static str) {
        if let Ok(balance) = self.provider.get_balance(self.operator_address).await {
            let balance_str = format_ether(balance);
            if let Ok(val) = balance_str.parse::<f64>() {
                L1_SENDER_METRICS.balance[&command_name].set(val);
            }
        }
        if let Ok(nonce) = self
            .provider
            .get_transaction_count(self.operator_address)
            .await
        {
            L1_SENDER_METRICS.nonce[&command_name].set(nonce);
        }
    }
}

// ==============================================================================
// Internal Poll Outcome
// ==============================================================================

enum PollOutcome {
    Receipt(alloy::rpc::types::TransactionReceipt),
    TimedOut,
}
