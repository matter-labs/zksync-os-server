use alloy::providers::ProviderCall;
use alloy::rpc::client::RpcCall;
use alloy::rpc::json_rpc::{RpcRecv, RpcSend};
use alloy::transports::TransportError;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// Per-call retry limit override consumed by the node provider retry middleware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcRetryLimit {
    /// Retry at most this many times after the initial attempt.
    Attempts(u32),
    /// Keep retrying retryable errors until the call succeeds.
    Infinite,
}

/// Data passed to a per-call retry callback before the next backoff sleep.
#[derive(Debug)]
pub struct RpcRetryEvent<'a> {
    pub call_name: &'static str,
    pub retry_number: u32,
    pub elapsed: Duration,
    pub backoff: Duration,
    pub error: &'a TransportError,
}

pub type RpcRetryCallback = Arc<dyn for<'a> Fn(&RpcRetryEvent<'a>) + Send + Sync>;

/// Per-call retry policy override consumed by the node provider retry middleware.
///
/// Without this marker, provider-level config is used.
#[derive(Clone)]
pub struct RpcRetryOverride {
    pub limit: RpcRetryLimit,
    pub backoff: Option<Duration>,
    pub call_name: &'static str,
    pub on_retry: Option<RpcRetryCallback>,
}

impl fmt::Debug for RpcRetryOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcRetryOverride")
            .field("limit", &self.limit)
            .field("backoff", &self.backoff)
            .field("call_name", &self.call_name)
            .field("has_on_retry", &self.on_retry.is_some())
            .finish()
    }
}

impl RpcRetryOverride {
    pub const fn infinite(call_name: &'static str) -> Self {
        Self {
            limit: RpcRetryLimit::Infinite,
            backoff: None,
            call_name,
            on_retry: None,
        }
    }

    pub fn on_retry(
        mut self,
        callback: impl for<'a> Fn(&RpcRetryEvent<'a>) + Send + Sync + 'static,
    ) -> Self {
        self.on_retry = Some(Arc::new(callback));
        self
    }

    pub const fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = Some(backoff);
        self
    }
}

/// Adds per-call retry metadata to prepared Alloy RPC calls.
pub trait RpcRetryExt: Sized {
    fn with_retry_override(self, override_: RpcRetryOverride) -> Self;

    fn with_infinite_retries(self, call_name: &'static str) -> Self {
        self.with_retry_override(RpcRetryOverride::infinite(call_name))
    }
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
