use super::metrics::METRICS;
use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::{TransportError, TransportFut};
use std::task::{Context, Poll};
use std::time::Instant;
use tower::{Layer, Service};

const BATCH_REQUEST_METHOD: &str = "batch_request";

#[derive(Debug, Clone, Copy)]
pub(super) struct LatencyLayer;

impl<S> Layer<S> for LatencyLayer {
    type Service = LatencyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LatencyService { inner }
    }
}

#[derive(Debug, Clone)]
pub(super) struct LatencyService<S> {
    inner: S,
}

impl<S> Service<RequestPacket> for LatencyService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Send
        + 'static
        + Clone,
    S::Future: Send + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let method = match &request {
            RequestPacket::Single(request) => request.method().to_owned(),
            RequestPacket::Batch(_) => BATCH_REQUEST_METHOD.to_owned(),
        };
        let inner = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, inner);
        Box::pin(async move {
            let started_at = Instant::now();
            let result = inner.call(request).await;
            METRICS.response_time[&method].observe(started_at.elapsed());
            result
        })
    }
}
