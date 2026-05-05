use super::metrics::METRICS;
use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::layers::{RateLimitRetryPolicy, RetryPolicy};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::sleep;
use tower::{Layer, Service};
use tracing::trace;

const MAX_RETRIES: u32 = 2;
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
pub(super) struct RetryLayer;

impl<S> Layer<S> for RetryLayer {
    type Service = RetryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RetryService { inner }
    }
}

#[derive(Debug, Clone)]
pub(super) struct RetryService<S> {
    inner: S,
}

impl<S> RetryService<S> {
    fn should_retry(error: &TransportError) -> bool {
        if RateLimitRetryPolicy::default().should_retry(error) {
            return true;
        }
        match error {
            TransportError::Transport(TransportErrorKind::HttpError(e)) => {
                // By default, only 429 and 503 are considered retryable; we also observe intermittent
                // 500 and 502 on Alchemy that are very likely retriable.
                e.status == 500 || e.status == 502
            }
            TransportError::Transport(TransportErrorKind::Custom(e)) => {
                let msg = e.to_string();
                // Internal `reqwest` error that can occur when node experiences intermittent
                // networking issues.
                msg.contains("error sending request")
            }
            TransportError::ErrorResp(e) => {
                // Internal error as observed on Infura
                e.code == -32603
            }
            _ => false,
        }
    }

    fn backoff_hint(error: &TransportError) -> Option<Duration> {
        RateLimitRetryPolicy::default().backoff_hint(error)
    }
}

impl<S> Service<RequestPacket> for RetryService<S>
where
    S: Service<RequestPacket, Future = TransportFut<'static>, Error = TransportError>
        + Send
        + 'static
        + Clone,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let inner = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, inner);
        Box::pin(async move {
            let mut retry_number = 0;
            loop {
                let err;
                let res = inner.call(request.clone()).await;

                match res {
                    Ok(res) => {
                        if let Some(e) = res.as_error() {
                            err = TransportError::ErrorResp(e.clone())
                        } else {
                            return Ok(res);
                        }
                    }
                    Err(e) => err = e,
                }

                if Self::should_retry(&err) {
                    retry_number += 1;
                    if retry_number > MAX_RETRIES {
                        return Err(TransportErrorKind::custom_str(&format!(
                            "Max retries exceeded {err}"
                        )));
                    }
                    METRICS.retry_count.inc();
                    trace!(%err, "retrying request");

                    let backoff = Self::backoff_hint(&err).unwrap_or(INITIAL_BACKOFF);
                    sleep(backoff).await;
                } else {
                    return Err(err);
                }
            }
        })
    }
}
