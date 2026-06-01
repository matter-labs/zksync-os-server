use super::ProviderKind;
use super::metrics::METRICS;
use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::layers::{RateLimitRetryPolicy, RetryPolicy};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tower::Service;
use zksync_os_alloy_ext::retry::{RpcRetryOverride, scoped_rpc_retry_override};

/// Retry RPC requests & track retry count metric
#[derive(Debug, Clone)]
pub(super) struct RetryService<S> {
    pub(super) inner: S,
    pub(super) provider: ProviderKind,
    pub(super) max_retries: u32,
    pub(super) backoff: Duration,
}

impl<S> RetryService<S> {
    fn retry_override(request: &RequestPacket) -> Option<&RpcRetryOverride> {
        let request = request.as_single()?;
        request.meta().extensions().get::<RpcRetryOverride>()
    }

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
        let provider = self.provider;
        let mut max_retries = self.max_retries;
        let mut backoff = self.backoff;
        let mut call_names = request
            .method_names()
            .fold(String::new(), |acc, name| acc + "_" + name);

        let retry_override = Self::retry_override(&request)
            .cloned()
            .or_else(scoped_rpc_retry_override);
        Box::pin(async move {
            if let Some(override_) = retry_override.clone() {
                if let Some(max_retries_override) = override_.limit {
                    max_retries = max_retries_override;
                }
                if let Some(backoff_override) = override_.backoff {
                    backoff = backoff_override;
                }
                call_names = override_.call_context.to_owned() + call_names.as_str();
            }
            let started_at = Instant::now();
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

                let should_retry = retry_override
                    .as_ref()
                    .is_some_and(|override_| override_.retry_all_errors)
                    || Self::should_retry(&err);
                if should_retry {
                    retry_number += 1;
                    if retry_number > max_retries {
                        return Err(TransportErrorKind::custom_str(&format!(
                            "Max retries exceeded {err}"
                        )));
                    }
                    METRICS[&provider].retry_count.inc();

                    let elapsed_secs = started_at.elapsed().as_secs();

                    if retry_number % 10 == 0 {
                        tracing::warn!(
                            "Query {call_names} was retried {retry_number} times, time elapsed: {elapsed_secs} seconds. Latest error: {err}"
                        );
                    }

                    sleep(Self::backoff_hint(&err).unwrap_or(backoff)).await;
                } else {
                    return Err(err);
                }
            }
        })
    }
}
