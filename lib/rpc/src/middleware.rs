use std::time::{Duration, Instant};

use crate::metrics::API_METRICS;
use jsonrpsee::core::middleware::{Batch, Notification};
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::Request;

#[derive(Debug)]
pub enum CallKind {
    Call,
    Notification,
    Batch,
}

#[derive(Clone)]
pub struct Monitoring {
    inner: RpcService,
}

impl Monitoring {
    pub fn new(inner: RpcService) -> Self {
        Self { inner }
    }
}

impl RpcServiceT for Monitoring {
    type MethodResponse = <RpcService as RpcServiceT>::MethodResponse;
    type NotificationResponse = <RpcService as RpcServiceT>::NotificationResponse;
    type BatchResponse = <RpcService as RpcServiceT>::BatchResponse;

    fn call<'a>(
        &self,
        request: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let method = request.method_name().to_owned();
        let fut = self.inner.call(request);

        async move {
            let started = Instant::now();
            let out = fut.await;
            let output_size = out.as_json().get().len();

            log_and_report(CallKind::Call, &method, started.elapsed(), output_size);
            out
        }
    }

    fn batch<'a>(
        &self,
        requests: Batch<'a>,
    ) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let fut = self.inner.batch(requests);

        async move {
            let started = Instant::now();
            let out = fut.await;
            let output_size = out.as_json().get().len();

            log_and_report(CallKind::Batch, "batch", started.elapsed(), output_size);
            out
        }
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let method = n.method_name().to_owned();
        let fut = self.inner.notification(n);

        async move {
            let started = Instant::now();
            let out = fut.await;
            let output_size = out.as_json().get().len();

            log_and_report(
                CallKind::Notification,
                &method,
                started.elapsed(),
                output_size,
            );
            out
        }
    }
}

fn log_and_report(kind: CallKind, method: &str, elapsed: Duration, output_size_bytes: usize) {
    API_METRICS.response_time[method].observe(elapsed);
    API_METRICS.response_size[method].observe(output_size_bytes);

    tracing::debug!(
        target: "rpc::monitoring",
        kind = ?kind,
        method,
        elapsed = ?elapsed,
        output_size_bytes,
        "rpc call completed"
    );
}
