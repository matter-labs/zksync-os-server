use alloy::network::Ethereum;
use alloy::providers::EthCall;
use alloy::rpc::json_rpc::RpcRecv;

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
