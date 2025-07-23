use alloy::network::Ethereum;
use alloy::providers::ext::DebugApi;
use alloy::providers::{EthCall, PendingTransactionBuilder};
use alloy::rpc::json_rpc::RpcRecv;
use alloy::rpc::types::TransactionReceipt;
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};

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
}

impl ReceiptAssert for PendingTransactionBuilder<Ethereum> {
    async fn expect_successful_receipt(self) -> anyhow::Result<TransactionReceipt> {
        let provider = self.provider().clone();
        let receipt = self.get_receipt().await?;
        if !receipt.status() {
            tracing::error!(?receipt, "transaction failed");
            let call_frame = provider
                .debug_trace_transaction(
                    receipt.transaction_hash,
                    GethDebugTracingOptions::call_tracer(CallConfig::default()),
                )
                .await?
                .try_into_call_frame()
                .expect("failed to convert call frame; should never happen");
            tracing::error!(?call_frame, "failed call frame");
            anyhow::bail!("transaction failed when it was expected to succeed");
        }

        Ok(receipt)
    }
}
