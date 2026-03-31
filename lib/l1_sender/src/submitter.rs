use crate::commands::{L1SenderCommand, SendToL1};
use crate::config::L1SenderConfig;
use crate::error::{is_nonce_too_low, is_transient};
use crate::metrics::{L1SenderState, L1_SENDER_METRICS};
use crate::types::{Backoff, GasParams, InFlightItem, InFlightTx, ResubmitRequest};
use alloy::consensus::BlobTransactionValidationError;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::eips::{BlockId, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::{Address, TxHash};
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{Provider, WalletProvider};
use alloy::rpc::types::TransactionRequest;
use anyhow::Context;
use tokio::sync::mpsc;
use zksync_os_observability::ComponentStateHandle;
use zksync_os_pipeline::PeekableReceiver;

// ==============================================================================
// Resubmission Decision
// ==============================================================================

/// Outcome returned by `resubmission_action`.
#[derive(Debug, PartialEq)]
pub(crate) enum ResubmitAction {
    /// Fees rose enough — send a replacement tx with the same nonce.
    SendReplacement,
    /// Fees have not risen enough — re-watch the original tx hash.
    RewatchOriginal,
}

/// Pure decision function: given old and freshly-estimated gas params, decide
/// whether to submit a replacement transaction or re-watch the original hash.
///
/// A replacement is warranted only when *every* fee dimension is at least 110%
/// of the previous value (EIP-1559 mempool replacement rule).
pub(crate) fn resubmission_action(old: &GasParams, fresh_estimate: &GasParams) -> ResubmitAction {
    if old.is_sufficient_replacement(fresh_estimate) {
        ResubmitAction::SendReplacement
    } else {
        ResubmitAction::RewatchOriginal
    }
}

// ==============================================================================
// Submitter
// ==============================================================================

/// Responsible for all L1 submission logic:
///
/// - Reads commands from `inbound` (and resubmit requests from `resubmit_rx`,
///   which take priority).
/// - Estimates gas, enforces fee caps, and sends raw transactions.
/// - Puts each submitted transaction into `in_flight_tx` for the Watcher.
/// - On resubmission: either sends a replacement tx (same nonce, higher fees)
///   or re-wraps the original hash so the Watcher re-watches it.
pub(crate) struct Submitter<F, P, Input>
where
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum>,
    Input: SendToL1,
{
    pub inbound: PeekableReceiver<L1SenderCommand<Input>>,
    pub in_flight_tx: mpsc::Sender<InFlightItem<Input>>,
    pub resubmit_rx: mpsc::Receiver<ResubmitRequest<Input>>,
    pub to_address: Address,
    pub provider: FillProvider<F, P>,
    pub config: L1SenderConfig<Input>,
    pub gateway: bool,
    pub operator_address: Address,
    pub next_nonce: u64,
    pub latency_tracker: ComponentStateHandle<L1SenderState>,
}

impl<F, P, Input> Submitter<F, P, Input>
where
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum> + Clone,
    Input: SendToL1 + Send + 'static,
{
    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            self.latency_tracker.enter_state(L1SenderState::WaitingRecv);

            // Resubmit requests take priority over new commands so that a
            // timed-out tx is replaced before the pipeline moves on.
            let work = tokio::select! {
                biased;
                resubmit = self.resubmit_rx.recv() => match resubmit {
                    Some(req) => Work::Resubmit(req),
                    None => {
                        tracing::info!("resubmit channel closed");
                        return Ok(());
                    }
                },
                cmd = self.inbound.recv() => match cmd {
                    Some(c) => Work::New(c),
                    None => {
                        tracing::info!(command_name = Input::NAME, "inbound channel closed");
                        return Ok(());
                    }
                },
            };

            match work {
                Work::New(L1SenderCommand::Passthrough(envelope)) => {
                    tracing::info!(
                        command_name = Input::NAME,
                        batch_number = envelope.batch_number(),
                        "Not actually sending to L1, just passing through",
                    );
                    self.in_flight_tx
                        .send(InFlightItem::Passthrough(envelope))
                        .await
                        .map_err(|_| anyhow::anyhow!("in_flight channel closed"))?;
                }
                Work::New(L1SenderCommand::SendToL1(cmd)) => {
                    self.submit_new(cmd).await?;
                }
                Work::Resubmit(req) => {
                    self.handle_resubmit(req).await?;
                }
            }
        }
    }

    // ==============================================================================
    // New Transaction Submission
    // ==============================================================================

    /// Estimate gas params, enforce caps, build and send a new transaction.
    async fn submit_new(&mut self, mut command: Input) -> anyhow::Result<()> {
        self.latency_tracker.enter_state(L1SenderState::SendingToL1);
        let range = Input::display_range(std::slice::from_ref(&command));
        tracing::info!(command_name = Input::NAME, range, "sending L1 transaction");

        let gas_params = self.estimate_gas_within_caps().await?;
        let (tx_hash, nonce, gas_params_used) =
            self.build_and_send(&command, &gas_params, None).await?;

        command
            .as_mut()
            .iter_mut()
            .for_each(|env| env.set_stage(Input::SENT_STAGE));

        self.in_flight_tx
            .send(InFlightItem::Tx(InFlightTx {
                tx_hash,
                gas_params: gas_params_used,
                command,
                nonce,
            }))
            .await
            .map_err(|_| anyhow::anyhow!("in_flight channel closed"))?;

        Ok(())
    }

    // ==============================================================================
    // Resubmission Handling
    // ==============================================================================

    /// Handle a resubmission request from the Watcher.
    ///
    /// If fresh estimates show a ≥10% fee bump: send a replacement transaction
    /// with the same nonce.  Otherwise: re-wrap the original hash so the Watcher
    /// continues watching the original transaction.
    async fn handle_resubmit(&mut self, req: ResubmitRequest<Input>) -> anyhow::Result<()> {
        tracing::info!(
            command_name = Input::NAME,
            tx_hash = ?req.original_tx_hash,
            nonce = req.nonce,
            "handling resubmission request",
        );

        let fresh = self.estimate_gas_within_caps().await?;

        let item = match resubmission_action(&req.original_gas_params, &fresh) {
            ResubmitAction::SendReplacement => {
                tracing::info!(
                    command_name = Input::NAME,
                    nonce = req.nonce,
                    "fees rose enough — sending replacement transaction",
                );
                let (tx_hash, nonce, gas_params_used) =
                    self.build_and_send(&req.command, &fresh, Some(req.nonce)).await?;
                InFlightItem::Tx(InFlightTx {
                    tx_hash,
                    gas_params: gas_params_used,
                    command: req.command,
                    nonce,
                })
            }
            ResubmitAction::RewatchOriginal => {
                tracing::info!(
                    command_name = Input::NAME,
                    tx_hash = ?req.original_tx_hash,
                    "fees have not risen enough — re-watching original tx",
                );
                InFlightItem::Tx(InFlightTx {
                    tx_hash: req.original_tx_hash,
                    gas_params: req.original_gas_params,
                    command: req.command,
                    nonce: req.nonce,
                })
            }
        };

        self.in_flight_tx
            .send(item)
            .await
            .map_err(|_| anyhow::anyhow!("in_flight channel closed"))?;

        Ok(())
    }

    // ==============================================================================
    // Gas Estimation
    // ==============================================================================

    /// Estimate EIP-1559 (and optionally blob) fees, blocking with backoff if
    /// estimates exceed configured caps.
    async fn estimate_gas_within_caps(&mut self) -> anyhow::Result<GasParams> {
        let mut backoff = Backoff::new();
        loop {
            let gas_params = match self.estimate_gas_params().await {
                Ok(p) => p,
                Err(e) if is_transient(&e) => {
                    tracing::warn!(
                        error = %e,
                        delay = ?backoff.current(),
                        "transient error estimating gas, retrying"
                    );
                    backoff.wait().await;
                    backoff.advance();
                    continue;
                }
                Err(e) => return Err(e),
            };
            backoff.reset();

            // Check fee caps — enter a blocked state if exceeded rather than
            // sending a transaction that violates operator cost constraints.
            if gas_params.max_fee_per_gas > self.config.max_fee_per_gas_wei {
                tracing::warn!(
                    estimated = gas_params.max_fee_per_gas,
                    cap = self.config.max_fee_per_gas_wei,
                    "gas fee exceeds cap, waiting 60s before re-estimating",
                );
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
            if let Some(blob_fee) = gas_params.max_fee_per_blob_gas {
                if blob_fee > self.config.max_fee_per_blob_gas_wei {
                    tracing::warn!(
                        estimated = blob_fee,
                        cap = self.config.max_fee_per_blob_gas_wei,
                        "blob fee exceeds cap, waiting 60s before re-estimating",
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    continue;
                }
            }

            return Ok(gas_params);
        }
    }

    async fn estimate_gas_params(&self) -> anyhow::Result<GasParams> {
        // Return the raw network estimate — do NOT clamp here.
        // estimate_gas_within_caps() compares raw values against the config caps
        // and blocks if they are exceeded; clamping before the check would
        // prevent the cap-blocking logic from ever triggering.
        let eip1559_est = self
            .provider
            .estimate_eip1559_fees()
            .await
            .map_err(anyhow::Error::new)?;
        L1_SENDER_METRICS.report_l1_eip_1559_estimation(eip1559_est)?;

        Ok(GasParams {
            max_fee_per_gas: eip1559_est.max_fee_per_gas,
            max_priority_fee_per_gas: eip1559_est.max_priority_fee_per_gas,
            // blob fee is fetched in build_and_send only when the command has a sidecar
            max_fee_per_blob_gas: None,
        })
    }

    // ==============================================================================
    // Transaction Building and Sending
    // ==============================================================================

    /// Build a signed transaction envelope and send it.  Returns `(tx_hash, nonce, gas_params_used)`.
    ///
    /// `explicit_nonce`: when `Some`, sets the nonce directly (used for replacement txs).
    /// When `None`, the nonce is taken from `self.next_nonce` and incremented on success.
    ///
    /// `gas_params` must already be within configured caps (ensured by
    /// `estimate_gas_within_caps`).
    ///
    /// The returned `GasParams` reflects the actual fees used — including the blob fee
    /// filled in for EIP-4844 transactions — so the Watcher can pass them back for an
    /// accurate resubmission comparison.
    async fn build_and_send(
        &mut self,
        command: &Input,
        gas_params: &GasParams,
        explicit_nonce: Option<u64>,
    ) -> anyhow::Result<(TxHash, u64, GasParams)> {
        let nonce = explicit_nonce.unwrap_or(self.next_nonce);

        let mut tx_request = TransactionRequest::default()
            .with_from(self.operator_address)
            .with_max_fee_per_gas(gas_params.max_fee_per_gas)
            .with_max_priority_fee_per_gas(gas_params.max_priority_fee_per_gas)
            // Default value for `max_aggregated_tx_gas` from zksync-era — should always be enough.
            .with_gas_limit(15_000_000)
            .with_nonce(nonce)
            .with_to(self.to_address)
            .with_input(command.solidity_call(self.gateway, &self.operator_address));

        // Build the final GasParams, filling in the blob fee when a sidecar is attached.
        // This value is returned to callers so they can store it for accurate resubmission
        // decisions — the raw `gas_params` arg has no blob fee and must not be used instead.
        let mut gas_params_with_blob = gas_params.clone();

        // Attach blob sidecar when the command provides one (EIP-4844 commit txs).
        // We use the configured cap rather than the raw estimate to avoid sending
        // a transaction that immediately violates the operator's cost constraints.
        if let Some(blob_sidecar) = command.blob_sidecar() {
            let fee_per_blob_gas = self
                .provider
                .get_blob_base_fee()
                .await
                .map_err(anyhow::Error::new)
                .context("get blob base fee")?;
            L1_SENDER_METRICS.report_blob_base_fee(fee_per_blob_gas)?;
            let max_fee_per_blob_gas = self.config.max_fee_per_blob_gas_wei;
            if fee_per_blob_gas > max_fee_per_blob_gas {
                tracing::warn!(
                    max_fee_per_blob_gas,
                    fee_per_blob_gas,
                    "L1 sender's configured maxFeePerBlobGas is lower than network estimate",
                );
            }
            tx_request.set_max_fee_per_blob_gas(max_fee_per_blob_gas);
            tx_request.set_blob_sidecar(blob_sidecar);
            gas_params_with_blob.max_fee_per_blob_gas = Some(max_fee_per_blob_gas);
        }

        // Fill remaining fields (chain id, access lists, etc.) via the provider
        // and convert to a signed pooled envelope ready for broadcast.
        let envelope = self
            .provider
            .fill(tx_request)
            .await
            .context("fill transaction")?
            .try_into_envelope()
            .context("convert to envelope")?
            .try_into_pooled()
            .context("convert to pooled transaction")?;

        let pending_block = self
            .provider
            .get_block(BlockId::pending())
            .await
            .map_err(anyhow::Error::new)?
            .expect("L1 provider must always expose a pending block");

        // Upgrade the blob sidecar to EIP-7594 format once the Fusaka upgrade
        // is active.  Before Fusaka, keep the standard EIP-4844 format because
        // anvil does not yet support EIP-7594 blobs.
        // TODO: remove the timestamp guard once anvil supports EIP-7594
        //       (see https://github.com/foundry-rs/foundry/issues/12222)
        let tx = if self.config.fusaka_upgrade_timestamp <= pending_block.header.timestamp {
            envelope.try_map_eip4844(|tx| {
                tx.try_map_sidecar(|sidecar| {
                    Ok::<_, BlobTransactionValidationError>(BlobTransactionSidecarVariant::Eip7594(
                        sidecar.try_into_eip7594()?,
                    ))
                })
            })?
        } else {
            envelope
        };

        // Send the raw transaction, retrying on transient RPC errors and
        // recovering from nonce-too-low by re-fetching the current nonce.
        //
        // `RpcError` doesn't implement `Clone`, so we convert to `anyhow::Error`
        // once and then branch on it via `is_nonce_too_low` / `is_transient`.
        let mut backoff = Backoff::new();
        let tx_hash = loop {
            match self
                .provider
                .send_raw_transaction(&tx.encoded_2718())
                .await
                .map_err(anyhow::Error::new)
            {
                Ok(pending) => {
                    let hash = *pending.tx_hash();
                    break hash;
                }
                Err(e) if is_nonce_too_low(&e) => {
                    tracing::warn!(nonce, "nonce too low, re-fetching");
                    self.next_nonce = self
                        .provider
                        .get_transaction_count(self.operator_address)
                        .await
                        .map_err(anyhow::Error::new)?;
                    return Err(e).context("nonce too low on resubmission — caller must retry");
                }
                Err(e) if is_transient(&e) => {
                    tracing::warn!(
                        error = %e,
                        delay = ?backoff.current(),
                        "transient send error, retrying"
                    );
                    backoff.wait().await;
                    backoff.advance();
                }
                Err(e) => return Err(e).context("send_raw_transaction"),
            }
        };

        L1_SENDER_METRICS.nonce[&Input::NAME].set(nonce);

        // Only advance the stored nonce when we allocated it ourselves.
        // For replacement transactions the caller supplied an explicit nonce,
        // so `self.next_nonce` is already correct.
        if explicit_nonce.is_none() {
            self.next_nonce += 1;
        }

        Ok((tx_hash, nonce, gas_params_with_blob))
    }
}

// ==============================================================================
// Internal Work Enum
// ==============================================================================

enum Work<Input: SendToL1> {
    New(L1SenderCommand<Input>),
    Resubmit(ResubmitRequest<Input>),
}

// ==============================================================================
// Tests
// ==============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GasParams;

    fn params(fee: u128, priority: u128) -> GasParams {
        GasParams {
            max_fee_per_gas: fee,
            max_priority_fee_per_gas: priority,
            max_fee_per_blob_gas: None,
        }
    }

    fn blob_params(fee: u128, priority: u128, blob: u128) -> GasParams {
        GasParams {
            max_fee_per_gas: fee,
            max_priority_fee_per_gas: priority,
            max_fee_per_blob_gas: Some(blob),
        }
    }

    #[test]
    fn sends_replacement_when_fees_sufficiently_higher() {
        assert_eq!(
            resubmission_action(&params(100, 10), &params(120, 15)),
            ResubmitAction::SendReplacement
        );
    }

    #[test]
    fn rewatches_original_when_fees_not_risen_enough() {
        assert_eq!(
            resubmission_action(&params(100, 10), &params(105, 11)),
            ResubmitAction::RewatchOriginal
        );
    }

    #[test]
    fn rewatches_original_when_fees_unchanged() {
        let p = params(100, 10);
        assert_eq!(
            resubmission_action(&p, &p.clone()),
            ResubmitAction::RewatchOriginal
        );
    }

    #[test]
    fn sends_replacement_for_blob_tx_when_all_fees_sufficient() {
        assert_eq!(
            resubmission_action(&blob_params(100, 10, 50), &blob_params(110, 11, 55)),
            ResubmitAction::SendReplacement
        );
    }

    #[test]
    fn rewatches_original_for_blob_tx_when_blob_fee_insufficient() {
        assert_eq!(
            resubmission_action(&blob_params(100, 10, 50), &blob_params(110, 11, 54)),
            ResubmitAction::RewatchOriginal
        );
    }

    #[test]
    fn resubmit_uses_fresh_params_not_old() {
        let old = params(100, 10);
        assert_eq!(
            resubmission_action(&old, &old.clone()),
            ResubmitAction::RewatchOriginal
        );
        assert_eq!(
            resubmission_action(&old, &params(115, 12)),
            ResubmitAction::SendReplacement
        );
    }
}
