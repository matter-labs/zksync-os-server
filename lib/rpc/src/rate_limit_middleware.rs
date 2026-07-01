use crate::limits::LoggingLimiter;
use jsonrpsee::MethodResponse;
use jsonrpsee::core::middleware::{Batch, Notification};
use jsonrpsee::core::to_json_raw_value;
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::Request;
use jsonrpsee::types::error::ErrorObject;
use serde::Serialize;
use std::sync::Arc;

/// EIP-1474 "Limit exceeded" — the de facto Ethereum rate-limit error code used by Infura, Alchemy, etc.
const RATE_LIMIT_ERROR_CODE: i32 = -32005;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RetryData {
    retry_after_ms: u64,
}

fn rate_limited_err(retry_after_ms: u64) -> ErrorObject<'static> {
    let data = to_json_raw_value(&RetryData { retry_after_ms }).expect("infallible serialization");
    ErrorObject::owned(RATE_LIMIT_ERROR_CODE, "Too many requests", Some(data))
}

/// JSON-RPC middleware that enforces per-method rate limits.
#[derive(Clone)]
pub(crate) struct RateLimiting<S = RpcService> {
    inner: S,
    limiter: Arc<LoggingLimiter>,
}

impl<S> RateLimiting<S> {
    pub(crate) fn new(inner: S, limiter: Arc<LoggingLimiter>) -> Self {
        Self { inner, limiter }
    }
}

impl<S> RpcServiceT for RateLimiting<S>
where
    S: RpcServiceT<
            MethodResponse = MethodResponse,
            NotificationResponse = MethodResponse,
            BatchResponse = MethodResponse,
        > + Clone
        + Send
        + 'static,
{
    type MethodResponse = MethodResponse;
    type NotificationResponse = MethodResponse;
    type BatchResponse = MethodResponse;

    fn call<'a>(
        &self,
        request: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let retry_after_ms = self.limiter.check(request.method_name());
        let inner = self.inner.clone();
        async move {
            if let Some(ms) = retry_after_ms {
                let id = request.id.clone().into_owned();
                return MethodResponse::error(id, rate_limited_err(ms));
            }
            inner.call(request).await
        }
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let inner = self.inner.clone();
        async move { inner.batch(batch).await }
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let inner = self.inner.clone();
        async move { inner.notification(n).await }
    }
}
