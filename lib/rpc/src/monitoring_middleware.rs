use std::time::{Duration, Instant};

use crate::metrics::API_METRICS;
use jsonrpsee::core::middleware::{Batch, Notification};
use jsonrpsee::server::middleware::rpc::{RpcService, RpcServiceT};
use jsonrpsee::types::Request;

#[derive(Debug)]
pub enum CallKind {
    Call,
    Notification,
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
        let request_size = request.params.as_ref().map_or(0, |p| p.get().len());
        let fut = self.inner.call(request);

        async move {
            let started = Instant::now();
            let out = fut.await;
            let output_size = out.as_json().get().len();

            log_and_report(
                CallKind::Call,
                &method,
                started.elapsed(),
                request_size,
                output_size,
            );
            out
        }
    }

    fn batch<'a>(
        &self,
        requests: Batch<'a>,
    ) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let batch_size = requests.len();
        let batch_input_size: usize = requests
            .iter()
            .filter_map(|x| {
                if let Ok(req) = x {
                    Some(req.params().as_ref().map_or(0, |p| p.get().len()))
                } else {
                    None
                }
            })
            .sum();

        let request_counts = requests
            .iter()
            .filter_map(|x| {
                if let Ok(req) = x {
                    Some(req.method_name().to_owned())
                } else {
                    None
                }
            })
            .fold(std::collections::HashMap::new(), |mut acc, method| {
                *acc.entry(method).or_insert(0) += 1;
                acc
            });
        let fut = self.inner.batch(requests);

        async move {
            let started = Instant::now();
            let out = fut.await;
            let output_size = out.as_json().get().len();

            let elapsed = started.elapsed();
            API_METRICS.response_time["batch"].observe(elapsed);
            API_METRICS.request_size["batch"].observe(batch_input_size);
            API_METRICS.response_size["batch"].observe(output_size);
            for (method, count) in request_counts {
                API_METRICS.requests_in_batch_count[&method].observe(count);
            }

            tracing::debug!(
                target: "rpc::monitoring::batch",
                method = "batch",
                batch_size,
                elapsed = ?elapsed,
                batch_input_size,
                output_size,
                "rpc batch call completed"
            );
            out
        }
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let request_size = n.params.as_ref().map_or(0, |p| p.get().len());
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
                request_size,
                output_size,
            );
            out
        }
    }
}

/// Macro to statically dispatch debug logs to different targets based on the method name.
macro_rules! debug_dispatch {
    (
        targets: match $method:ident { $($method_arm:literal => $target_arm:literal,)* _ => $fallback:literal },
        fields: $fields:tt,
        message: $message:literal,
    ) => {
        match $method {
            $($method_arm => {
                tracing::debug!(
                    target: $target_arm,
                    $fields,
                    $message
                );
            })*
            _ => {
                tracing::debug!(
                    target: $fallback,
                    $fields,
                    $message
                );
            }
        }
    };
}

fn log_and_report(
    kind: CallKind,
    method: &str,
    elapsed: Duration,
    request_size: usize,
    output_size_bytes: usize,
) {
    API_METRICS.response_time[method].observe(elapsed);
    API_METRICS.request_size[method].observe(request_size);
    API_METRICS.response_size[method].observe(output_size_bytes);

    debug_dispatch!(
        targets: match method {
            "eth_call" => "rpc::monitoring::eth::call",
            "eth_sendRawTransaction" => "rpc::monitoring::eth::sendRawTransaction",
            "debug_traceTransaction" => "rpc::monitoring::debug::traceTransaction",
            _ => "rpc::monitoring::call"
        },
        fields: {
            method,
            ?kind,
            ?elapsed,
            request_size,
            output_size_bytes,
        },
        message: "rpc call completed",
    );
}
