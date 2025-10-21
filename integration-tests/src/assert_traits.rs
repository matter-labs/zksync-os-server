use alloy::eips::BlockId;
use alloy::network::{Ethereum, Network, ReceiptResponse};
use alloy::providers::ext::DebugApi;
use alloy::providers::{EthCall, PendingTransaction, PendingTransactionBuilder, Provider};
use alloy::rpc::json_rpc::RpcRecv;
use alloy::rpc::types::TransactionReceipt;
use alloy::rpc::types::trace::geth::{CallConfig, CallFrame, GethDebugTracingOptions};
use anyhow::Context;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

#[allow(async_fn_in_trait)]
pub trait EthCallAssert {
    async fn expect_to_fail(self, msg: &str);
}

impl<Resp: RpcRecv> EthCallAssert for EthCall<Ethereum, Resp> {
    async fn expect_to_fail(self, msg: &str) {
        let err = self
            .await
            .expect_err(&format!("`eth_call` should fail with error: {msg}"));
        assert!(
            err.to_string().contains(msg),
            "expected `eth_call` to fail with error '{msg}' but got: {err}",
        );
    }
}

#[allow(async_fn_in_trait)]
pub trait ReceiptAssert<N: Network> {
    async fn expect_successful_receipt(self) -> anyhow::Result<N::ReceiptResponse>;
    async fn expect_register(self) -> anyhow::Result<PendingTransaction>;
    async fn expect_to_execute(self) -> anyhow::Result<N::ReceiptResponse>;
    async fn expect_call_trace(self) -> anyhow::Result<CallFrame>;
}

impl<N: Network> ReceiptAssert<N> for PendingTransactionBuilder<N> {
    async fn expect_successful_receipt(self) -> anyhow::Result<N::ReceiptResponse> {
        let provider = self.provider().clone();
        let receipt = self
            .with_timeout(Some(DEFAULT_TIMEOUT))
            .get_receipt()
            .await?;
        if !receipt.status() {
            tracing::error!(?receipt, "Transaction failed");
            // Ignore error if `deubg_traceTransaction` is not implemented (which is currently the
            // case for zksync-os node).
            if let Ok(trace) = provider
                .debug_trace_transaction(
                    receipt.transaction_hash(),
                    GethDebugTracingOptions::call_tracer(CallConfig::default()),
                )
                .await
            {
                let call_frame = trace
                    .try_into_call_frame()
                    .expect("failed to convert call frame; should never happen");
                tracing::error!(?call_frame, "Failed call frame");
                anyhow::bail!("transaction failed when it was expected to succeed");
            }
        }

        Ok(receipt)
    }

    async fn expect_register(self) -> anyhow::Result<PendingTransaction> {
        Ok(self.with_timeout(Some(DEFAULT_TIMEOUT)).register().await?)
    }

    async fn expect_to_execute(self) -> anyhow::Result<N::ReceiptResponse> {
        let provider = self.provider().clone();
        let receipt = self.expect_successful_receipt().await?;
        let expected_block = receipt
            .block_number()
            .context("mined receipt is missing block number")?;
        // Wait until the expected block is executed.
        const POLL_INTERVAL: Duration = Duration::from_millis(100);
        let mut retries = DEFAULT_TIMEOUT.div_duration_f64(POLL_INTERVAL).floor() as u64;
        while retries > 0 {
            // Finalized block is mapped to the latest executed block.
            let executed_block = provider
                .get_block_number_by_id(BlockId::finalized())
                .await?
                .unwrap_or(0);
            if executed_block >= expected_block {
                tracing::debug!(executed_block, "expected block was executed");
                return Ok(receipt);
            } else {
                tracing::debug!(
                    executed_block,
                    expected_block,
                    "expected block was not executed yet, retrying..."
                );
                retries -= 1;
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
        Err(anyhow::anyhow!(
            "transaction was not executed on L1 in time"
        ))
    }

    async fn expect_call_trace(self) -> anyhow::Result<CallFrame> {
        let provider = self.provider().clone();
        let receipt = self
            .with_timeout(Some(DEFAULT_TIMEOUT))
            .get_receipt()
            .await?;
        let trace = provider
            .debug_trace_transaction(
                receipt.transaction_hash(),
                GethDebugTracingOptions::call_tracer(CallConfig::default()),
            )
            .await?;
        trace
            .try_into_call_frame()
            .context("failed to parse call trace")
    }
}

#[allow(async_fn_in_trait)]
pub trait ReceiptsAssert {
    async fn expect_successful_receipts(self) -> anyhow::Result<Vec<TransactionReceipt>>;
}

impl ReceiptsAssert for Vec<PendingTransactionBuilder<Ethereum>> {
    async fn expect_successful_receipts(self) -> anyhow::Result<Vec<TransactionReceipt>> {
        let receipts =
            futures::future::join_all(self.into_iter().map(|tx| tx.expect_successful_receipt()))
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;
        Ok(receipts)
    }
}
