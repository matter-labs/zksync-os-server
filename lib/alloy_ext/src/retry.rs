use alloy::providers::ProviderCall;
use alloy::rpc::client::RpcCall;
use alloy::rpc::json_rpc::{RpcRecv, RpcSend};
use std::fmt;
use std::future::Future;
use std::time::Duration;

/// Per-call retry policy override consumed by the node provider retry middleware.
///
/// Without this marker, provider-level config is used.
#[derive(Clone)]
pub struct RpcRetryOverride {
    pub limit: Option<u32>,
    pub backoff: Option<Duration>,
    pub retry_all_errors: bool,
    pub call_context: &'static str,
}

impl fmt::Debug for RpcRetryOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcRetryOverride")
            .field("limit", &self.limit)
            .field("backoff", &self.backoff)
            .field("retry_all_errors", &self.retry_all_errors)
            .field("call_context", &self.call_context)
            .finish()
    }
}

impl RpcRetryOverride {
    pub const fn new() -> Self {
        Self {
            limit: None,
            backoff: None,
            retry_all_errors: false,
            call_context: "",
        }
    }

    pub const fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    pub const fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = Some(backoff);
        self
    }

    pub const fn retry_all_errors(mut self) -> Self {
        self.retry_all_errors = true;
        self
    }

    pub const fn with_context(mut self, call_context: &'static str) -> Self {
        self.call_context = call_context;
        self
    }
}

impl Default for RpcRetryOverride {
    fn default() -> Self {
        Self::new()
    }
}

tokio::task_local! {
    static SCOPED_RPC_RETRY_OVERRIDE: RpcRetryOverride;
}

/// Applies a retry override to every RPC request issued while `future` is running.
///
/// This is useful for higher-level provider calls that do not expose the underlying
/// `RpcCall` where [`RpcRetryExt`] can attach metadata directly.
pub async fn with_scoped_rpc_retry_override<F>(override_: RpcRetryOverride, future: F) -> F::Output
where
    F: Future,
{
    SCOPED_RPC_RETRY_OVERRIDE.scope(override_, future).await
}

/// Returns the currently scoped retry override, if one is active.
pub fn scoped_rpc_retry_override() -> Option<RpcRetryOverride> {
    SCOPED_RPC_RETRY_OVERRIDE.try_with(Clone::clone).ok()
}

/// Adds per-call retry metadata to prepared Alloy RPC calls.
pub trait RpcRetryExt: Sized {
    fn with_retry_override(self, override_: RpcRetryOverride) -> Self;
}

impl<Params, Resp, Output, Map> RpcRetryExt for RpcCall<Params, Resp, Output, Map>
where
    Params: RpcSend,
    Map: FnOnce(Resp) -> Output,
{
    fn with_retry_override(self, override_: RpcRetryOverride) -> Self {
        self.map_meta(|mut meta| {
            meta.extensions_mut().insert(override_);
            meta
        })
    }
}

impl<Params, Resp, Output, Map> RpcRetryExt for ProviderCall<Params, Resp, Output, Map>
where
    Params: RpcSend,
    Resp: RpcRecv,
    Map: Fn(Resp) -> Output,
{
    fn with_retry_override(self, override_: RpcRetryOverride) -> Self {
        match self {
            Self::RpcCall(call) => Self::RpcCall(call.with_retry_override(override_)),
            other => other,
        }
    }
}
