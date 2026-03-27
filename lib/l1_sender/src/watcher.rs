use crate::batcher_model::{FriProof, SignedBatchEnvelope};
use crate::commands::SendToL1;
use crate::error::RecoverableReason;
use crate::metrics::{L1_SENDER_METRICS, L1SenderState};
use crate::{InFlightTx, reason_label, validate_tx_receipt_reverted};
use alloy::network::Ethereum;
use alloy::primitives::B256;
use alloy::providers::{PendingTransactionError, Provider, WatchTxError};
use alloy::rpc::types::TransactionReceipt;
use futures::StreamExt;
use futures::stream::FuturesOrdered;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateHandle;

// ==============================================================================
// Watcher
// ==============================================================================

/// Watches in-flight L1 transactions for inclusion, using `FuturesOrdered` to
/// poll all receipt futures concurrently while delivering results in nonce order.
///
/// On a confirmed receipt the command is forwarded downstream. On timeout or
/// transient polling error the command is sent back to the Submitter via the
/// resubmit channel for resubmission. Fatal errors (tx revert) return `Err`.
pub(crate) struct Watcher<Input, P>
where
    Input: SendToL1,
    P: Provider<Ethereum>,
{
    pub(crate) in_flight_rx: mpsc::Receiver<InFlightTx<Input>>,
    pub(crate) resubmit_tx: mpsc::Sender<Input>,
    pub(crate) outbound: mpsc::Sender<SignedBatchEnvelope<FriProof>>,
    pub(crate) provider: P,
    pub(crate) latency_tracker: ComponentStateHandle<L1SenderState>,
}

/// The resolved output of one receipt future: the original command, its tx hash
/// (for logging even after the future is consumed), and the polling result.
type WatchResult<Input> = (
    Input,
    B256,
    Result<TransactionReceipt, PendingTransactionError>,
);

impl<Input, P> Watcher<Input, P>
where
    Input: SendToL1 + Send + 'static,
    P: Provider<Ethereum> + Clone,
{
    pub async fn run(mut self) -> anyhow::Result<()> {
        // All in-flight receipt futures are polled here concurrently.
        // FuturesOrdered yields results in submission (nonce) order.
        let mut pending: FuturesOrdered<Pin<Box<dyn Future<Output = WatchResult<Input>> + Send>>> =
            FuturesOrdered::new();

        loop {
            tokio::select! {
                // A new in-flight tx arrived from the Submitter.
                result = self.in_flight_rx.recv() => {
                    match result {
                        Some(tx) => {
                            self.latency_tracker.enter_state(L1SenderState::WaitingL1Inclusion);
                            pending.push_back(Box::pin(async move {
                                let receipt_result = tx.receipt_future.await;
                                (tx.command, tx.tx_hash, receipt_result)
                            }));
                        }
                        None => {
                            // Submitter died — drain any futures that are already in flight.
                            break;
                        }
                    }
                }
                // The next receipt resolved (in nonce order).
                Some(watch_result) = pending.next(), if !pending.is_empty() => {
                    let (command, tx_hash, receipt_result) = watch_result;
                    self.handle_receipt(command, tx_hash, receipt_result).await?;
                }
            }
        }

        // Drain remaining futures after the Submitter's channel closed.
        while let Some((command, tx_hash, receipt_result)) = pending.next().await {
            self.handle_receipt(command, tx_hash, receipt_result)
                .await?;
        }

        Ok(())
    }

    async fn handle_receipt(
        &mut self,
        command: Input,
        tx_hash: B256,
        result: Result<TransactionReceipt, PendingTransactionError>,
    ) -> anyhow::Result<()> {
        match result {
            Ok(receipt) if receipt.status() => {
                // Tx confirmed — report metrics and forward downstream.
                self.latency_tracker.enter_state(L1SenderState::WaitingSend);
                L1_SENDER_METRICS.report_tx_receipt(&command, receipt);
                for mut envelope in command.into() {
                    envelope.set_stage(Input::MINED_STAGE);
                    self.outbound
                        .send(envelope)
                        .await
                        .map_err(|e| anyhow::anyhow!("outbound channel closed: {e}"))?;
                }
            }
            Ok(receipt) => {
                // Tx reverted on L1 — fatal, gas already burned.
                // validate_tx_receipt_reverted always returns Err, so we propagate it directly.
                validate_tx_receipt_reverted(&self.provider, &command, receipt).await?;
                unreachable!("validate_tx_receipt_reverted always returns Err");
            }
            Err(PendingTransactionError::TxWatcher(WatchTxError::Timeout)) => {
                // Receipt future timed out. Re-queue the command for resubmission.
                tracing::warn!(
                    ?tx_hash,
                    command_name = Input::NAME,
                    "tx timed out waiting for inclusion, re-queuing for resubmission"
                );
                L1_SENDER_METRICS.recoverable_errors[&reason_label(RecoverableReason::TxTimeout)]
                    .inc();
                self.resubmit_tx
                    .send(command)
                    .await
                    .map_err(|_| anyhow::anyhow!("resubmit channel closed (Submitter died)"))?;
            }
            Err(e) => {
                // Transient receipt polling error — re-queue for resubmission.
                tracing::warn!(
                    ?tx_hash,
                    ?e,
                    command_name = Input::NAME,
                    "transient error polling receipt, re-queuing for resubmission"
                );
                L1_SENDER_METRICS.transient_errors.inc();
                self.resubmit_tx
                    .send(command)
                    .await
                    .map_err(|_| anyhow::anyhow!("resubmit channel closed (Submitter died)"))?;
            }
        }
        Ok(())
    }
}
