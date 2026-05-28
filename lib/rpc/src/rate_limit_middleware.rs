use crate::config::RpcRateLimit;
use governor::clock::{Clock, DefaultClock};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use jsonrpsee::MethodResponse;
use jsonrpsee::core::middleware::{Batch, Notification};
use jsonrpsee::core::to_json_raw_value;
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::Request;
use jsonrpsee::types::error::ErrorObject;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

/// EIP-1474 "Limit exceeded" — the de facto Ethereum rate-limit error code used by Infura, Alchemy, etc.
const RATE_LIMIT_ERROR_CODE: i32 = -32005;

/// Pre-builds the shared limiter map from config.  Pass the returned `Arc` to every
/// `RateLimiting::new` call so all connections share the same token-bucket state.
pub(crate) fn build_limiters(
    limits: &[RpcRateLimit],
) -> Arc<HashMap<String, DefaultDirectRateLimiter>> {
    Arc::new(
        limits
            .iter()
            .map(|l| {
                (
                    l.method.clone(),
                    RateLimiter::direct(Quota::per_second(l.requests_per_second)),
                )
            })
            .collect(),
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RetryData {
    retry_after_ms: u64,
}

fn rate_limited_err(retry_after_ms: u64) -> ErrorObject<'static> {
    let data = to_json_raw_value(&RetryData { retry_after_ms }).expect("infallible serialization");
    ErrorObject::owned(RATE_LIMIT_ERROR_CODE, "Too many requests", Some(data))
}

/// JSON-RPC middleware that enforces per-method request rate limits globally across all connections.
///
/// Build the limiter map once with [`build_limiters`] and share it across connections via `Arc`.
/// Sit this layer inside `Monitoring` so rate-limited responses are counted in error metrics.
/// `Monitoring` decomposes batch requests by calling `call()` per entry, so batch items are
/// rate-limited automatically.  Any method absent from the map is unrestricted.
#[derive(Clone)]
pub(crate) struct RateLimiting<S = RpcService> {
    inner: S,
    limiters: Arc<HashMap<String, DefaultDirectRateLimiter>>,
}

impl<S> RateLimiting<S> {
    pub(crate) fn new(inner: S, limiters: Arc<HashMap<String, DefaultDirectRateLimiter>>) -> Self {
        Self { inner, limiters }
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
        // Check synchronously before the async block to avoid holding a borrow across an await.
        // Global limit ("*") is checked first; if it fires the per-method limiter is not touched.
        let now = DefaultClock::default().now();
        let not_until_global = self.limiters.get("*").and_then(|l| l.check().err());
        let not_until_method = if not_until_global.is_none() {
            self.limiters
                .get(request.method_name())
                .and_then(|l| l.check().err())
        } else {
            None
        };
        let retry_after_ms = not_until_global.or(not_until_method).map(|not_until| {
            not_until
                .wait_time_from(now)
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX)
        });
        let inner = self.inner.clone();
        async move {
            if let Some(ms) = retry_after_ms {
                return MethodResponse::error(
                    request.id.clone().into_owned(),
                    rate_limited_err(ms),
                );
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
