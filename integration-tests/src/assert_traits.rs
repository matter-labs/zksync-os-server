use alloy::network::Ethereum;
use alloy::providers::ext::DebugApi;
use alloy::providers::{EthCall, PendingTransaction, PendingTransactionBuilder};
use alloy::rpc::json_rpc::RpcRecv;
use alloy::rpc::types::TransactionReceipt;
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[allow(async_fn_in_trait)]
pub trait EthCallAssert {
    async fn expect_to_fail(self, msg: &str);
}

impl<Resp: RpcRecv> EthCallAssert for EthCall<Ethereum, Resp> {
    async fn expect_to_fail(self, msg: &str) {
        let err = self
            .await
            .expect_err(&format!("`eth_call` should fail with error: {}", msg));
        assert!(
            err.to_string().contains(msg),
            "expected `eth_call` to fail with error '{}' but got: {}",
            msg,
            err
        );
    }
}

#[allow(async_fn_in_trait)]
pub trait ReceiptAssert {
    async fn expect_successful_receipt(self) -> anyhow::Result<TransactionReceipt>;
    async fn expect_register(self) -> anyhow::Result<PendingTransaction>;
}

impl ReceiptAssert for PendingTransactionBuilder<Ethereum> {
    async fn expect_successful_receipt(self) -> anyhow::Result<TransactionReceipt> {
        let provider = self.provider().clone();
        let receipt = self
            .with_timeout(Some(DEFAULT_TIMEOUT))
            .get_receipt()
            .await?;
        if !receipt.status() {
            tracing::error!(?receipt, "transaction failed");
            // Ignore error if `deubg_traceTransaction` is not implemented (which is currently the
            // case for zksync-os node).
            if let Ok(trace) = provider
                .debug_trace_transaction(
                    receipt.transaction_hash,
                    GethDebugTracingOptions::call_tracer(CallConfig::default()),
                )
                .await
            {
                let call_frame = trace
                    .try_into_call_frame()
                    .expect("failed to convert call frame; should never happen");
                tracing::error!(?call_frame, "failed call frame");
                anyhow::bail!("transaction failed when it was expected to succeed");
            }
        }

        Ok(receipt)
    }

    async fn expect_register(self) -> anyhow::Result<PendingTransaction> {
        Ok(self.with_timeout(Some(DEFAULT_TIMEOUT)).register().await?)
    }
}
